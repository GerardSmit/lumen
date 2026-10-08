//! Embedded object transport on the browser's existing blocking pool/waker.
//! Queued tickets carry plain requests; only admitted byte envelopes start I/O.
use lumen_common::limits::{ByteBudget, ByteLease};
use lumen_html_js::object_loading::{
    ObjectFailure, ObjectRequest, ObjectResourceLoader, ObjectResponse,
};
use lumen_os::net::TcpCancellation;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::{mpsc, Arc};

pub(crate) const BODY_LIMIT: usize = lumen_html_js::object_loading::OBJECT_BODY_LIMIT;
type ResultData = Result<ObjectResponse, ObjectFailure>;
struct Ticket {
    result: Option<mpsc::Receiver<ResultData>>,
    cancelled: TcpCancellation,
}
pub(crate) struct Provider {
    pool: lumen_host::SpawnHandle,
    wake: super::RuntimeWaker,
    config: lumen_web::FetchConfig,
    budget: Arc<ByteBudget>,
    next: Cell<u64>,
    tickets: RefCell<HashMap<u64, Ticket>>,
    waiting: RefCell<VecDeque<(u64, ObjectRequest)>>,
}
impl Provider {
    pub(crate) fn new(
        pool: lumen_host::SpawnHandle,
        wake: super::RuntimeWaker,
        config: lumen_web::FetchConfig,
        budget: Arc<ByteBudget>,
    ) -> Self {
        Self {
            pool,
            wake,
            config,
            budget,
            next: Cell::new(0),
            tickets: RefCell::new(HashMap::new()),
            waiting: RefCell::new(VecDeque::new()),
        }
    }

    fn admit_waiting(&self) {
        loop {
            if self.waiting.borrow().is_empty() {
                return;
            }
            let Some(reservation) = self.budget.reserve(BODY_LIMIT) else {
                return;
            };
            let Some((id, request)) = self.waiting.borrow_mut().pop_front() else {
                return;
            };
            let (tx, result) = mpsc::channel();
            let cancelled = {
                let mut tickets = self.tickets.borrow_mut();
                let Some(ticket) = tickets.get_mut(&id) else {
                    continue;
                };
                ticket.result = Some(result);
                ticket.cancelled.clone()
            };
            let config = self.config.clone();
            let wake = self.wake.clone();
            if lumen_common::url::parse(&request.url, None)
                .is_ok_and(|url| matches!(url.scheme.as_str(), "data" | "blob" | "about"))
            {
                // Pure local resource processing has no OS work to defer. This
                // preserves the object algorithm's available-response branch.
                let result = fetch(&request, &config, &cancelled, reservation);
                if !cancelled.is_cancelled() {
                    let _ = tx.send(result);
                }
                wake.wake();
                continue;
            }
            self.pool.spawn_detached(Box::new(move || {
                let result = fetch(&request, &config, &cancelled, reservation);
                if !cancelled.is_cancelled() {
                    let _ = tx.send(result);
                }
                // Cancellation and response shrink can both release envelopes
                // needed by waiting tickets in another document's provider.
                wake.wake();
            }));
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        for ticket in self.tickets.get_mut().values() {
            ticket.cancelled.cancel();
        }
    }
}
impl ObjectResourceLoader for Provider {
    fn start(&self, request: ObjectRequest) -> Result<u64, String> {
        let id = self
            .next
            .get()
            .checked_add(1)
            .ok_or_else(|| "object ticket space exhausted".to_owned())?;
        let mut tickets = self.tickets.borrow_mut();
        tickets
            .try_reserve(1)
            .map_err(|_| "object ticket allocation".to_owned())?;
        let mut waiting = self.waiting.borrow_mut();
        waiting
            .try_reserve(1)
            .map_err(|_| "object waiting ticket allocation".to_owned())?;
        tickets.insert(
            id,
            Ticket {
                result: None,
                cancelled: TcpCancellation::default(),
            },
        );
        waiting.push_back((id, request));
        self.next.set(id);
        drop(waiting);
        drop(tickets);
        self.admit_waiting();
        Ok(id)
    }
    fn poll(&self, id: u64) -> Option<ResultData> {
        self.admit_waiting();
        let mut tickets = self.tickets.borrow_mut();
        let receiver = tickets.get(&id)?.result.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(ObjectFailure {
                message: "object transport stopped".into(),
                violations: Vec::new(),
            }),
        };
        tickets.remove(&id);
        Some(result)
    }
    fn cancel(&self, id: u64) {
        if let Some(ticket) = self.tickets.borrow_mut().remove(&id) {
            ticket.cancelled.cancel();
        }
        self.waiting
            .borrow_mut()
            .retain(|(ticket, _)| *ticket != id);
    }
}

fn fetch(
    request: &ObjectRequest,
    config: &lumen_web::FetchConfig,
    cancelled: &TcpCancellation,
    mut reservation: ByteLease,
) -> ResultData {
    let mut metadata = lumen_web::ScriptFetchMetadata {
        destination: request.destination,
        self_url: request.document_url.clone(),
        nonce: String::new(),
        integrity: String::new(),
        parser_inserted: request.parser_inserted,
        referrer: request.referrer.clone(),
        policies: request.policies.clone(),
        violations: Vec::new(),
    };
    let result = (|| {
        if cancelled.is_cancelled() {
            return Err("object request aborted".to_owned());
        }
        let url = lumen_common::url::parse(&request.url, None)?;
        if matches!(url.scheme.as_str(), "data" | "blob" | "about") {
            let decision = metadata
                .policies
                .check_resource_redirect(
                    &request.url,
                    &request.url,
                    &metadata.self_url,
                    metadata.destination,
                    "",
                    "",
                    request.parser_inserted,
                    0,
                )
                .map_err(|error| format!("object policy: {error:?}"))?;
            metadata.violations.extend(decision.violations);
            if decision.blocked {
                return Err("object blocked by Content Security Policy".into());
            }
            let (body, mime, creator_origin) = match url.scheme.as_str() {
                "data" => (
                    lumen_common::url::data_url_body_bounded(&request.url, BODY_LIMIT)
                        .map_err(|error| format!("object data URL: {error:?}"))?,
                    lumen_common::url::data_url_media_type(&request.url)
                        .map(|mime| mime.into_owned()),
                    None,
                ),
                "blob" => {
                    let resource = request
                        .local_resource
                        .as_ref()
                        .ok_or_else(|| "object Blob resource unavailable".to_owned())?;
                    if resource.bytes.len() > BODY_LIMIT {
                        return Err("object Blob exceeds resource byte limit".into());
                    }
                    (
                        resource.bytes.to_vec(),
                        Some(resource.mime.clone()),
                        resource.creator_origin.clone(),
                    )
                }
                "about" if url.path == "blank" => (Vec::new(), Some("text/html".into()), None),
                _ => return Err("object URL scheme unsupported".into()),
            };
            let response = lumen_common::http_body::SyncHttpResponse {
                status: 200,
                status_text: "OK".into(),
                url: request.url.clone(),
                headers: mime
                    .map(|mime| vec![("Content-Type".into(), mime)])
                    .unwrap_or_default(),
                body,
            };
            Ok((response, None, creator_origin))
        } else {
            let response = lumen_web::request_embedded_navigation_resource_with_config(
                &request.url,
                config,
                BODY_LIMIT,
                &mut metadata,
                cancelled,
            )?;
            let request_referrer = response.request_referrer;
            let response = response.response;
            Ok((
                lumen_common::http_body::SyncHttpResponse {
                    status: response.status,
                    status_text: response.status_text,
                    url: response.url,
                    headers: response.headers,
                    body: response.body,
                },
                request_referrer,
                None,
            ))
        }
    })();
    match result {
        Ok((response, request_referrer, creator_origin)) => {
            if !reservation.shrink_to(response.body.len()) {
                return Err(ObjectFailure {
                    message: "object response exceeds admitted bytes".into(),
                    violations: metadata.violations,
                });
            }
            Ok(ObjectResponse {
                response,
                request_referrer,
                creator_origin,
                reservation,
                violations: metadata.violations,
            })
        }
        Err(message) => Err(ObjectFailure {
            message,
            violations: metadata.violations,
        }),
    }
}
