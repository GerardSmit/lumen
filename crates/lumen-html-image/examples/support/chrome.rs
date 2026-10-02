use serde_json::{json, Value};
use std::{
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Chrome {
    process: Child,
    pub profile: PathBuf,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Chrome {
    pub fn launch(browser: &Path, output: &Path) -> Result<Self> {
        let profile = output.join(format!(
            "lumen-chrome-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&profile)?;
        let mut command = Command::new(browser);
        command
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--hide-scrollbars",
                "--no-first-run",
                "--allow-file-access-from-files",
                "--force-color-profile=srgb",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        Ok(Self {
            process: command.spawn()?,
            profile,
        })
    }

    pub fn connect(&mut self) -> Result<Cdp> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(port) = std::fs::read_to_string(self.profile.join("DevToolsActivePort")) {
                let mut lines = port.lines();
                let port = lines.next().ok_or("missing debugging port")?;
                let endpoint = lines.next().ok_or("missing debugging endpoint")?;
                let stream = TcpStream::connect_timeout(
                    &format!("127.0.0.1:{port}").parse()?,
                    Duration::from_secs(5),
                )?;
                stream.set_read_timeout(Some(Duration::from_secs(15)))?;
                stream.set_write_timeout(Some(Duration::from_secs(15)))?;
                let (socket, _) =
                    tungstenite::client(format!("ws://127.0.0.1:{port}{endpoint}"), stream)?;
                return Ok(Cdp {
                    socket,
                    id: 0,
                    session: None,
                });
            }
            if self.process.try_wait()?.is_some() {
                return Err("Chrome exited before opening CDP".into());
            }
            if Instant::now() >= deadline {
                return Err("Chrome CDP startup timed out".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub struct Cdp {
    socket: WebSocket<TcpStream>,
    id: u64,
    session: Option<String>,
}

impl Cdp {
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let mut request = json!({"id": self.id, "method": method, "params": params});
        if let Some(session) = &self.session {
            request["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(request.to_string().into()))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        for _ in 0..10000 {
            if Instant::now() >= deadline {
                return Err(format!("{method} timed out").into());
            }
            let message = self.socket.read()?;
            if let Message::Text(text) = message {
                let response: Value = serde_json::from_str(&text)?;
                if response["id"] == self.id {
                    if let Some(error) = response.get("error") {
                        return Err(format!("{method}: {error}").into());
                    }
                    return Ok(response["result"].clone());
                }
            }
        }
        Err(format!("{method}: excessive CDP messages").into())
    }

    pub fn capture(
        &mut self,
        fixture: &Path,
        screenshot: &Path,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.session = None;
        let target = self.call("Target.createTarget", json!({"url":"about:blank"}))?;
        let target = target["targetId"]
            .as_str()
            .ok_or("missing target")?
            .to_owned();
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
        )?;
        self.session = Some(
            attached["sessionId"]
                .as_str()
                .ok_or("missing session")?
                .to_owned(),
        );
        self.call("Page.enable", json!({}))?;
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":height,"deviceScaleFactor":1,"mobile":false}),
        )?;
        let path = fixture.to_string_lossy().replace('\\', "/");
        let path = path.strip_prefix("//?/").unwrap_or(&path);
        let url = format!(
            "file:///{}",
            lumen_common::codec::percent_encode(path.as_bytes(), |c| !c.is_ascii_alphanumeric()
                && !b"/:.-_~".contains(&c))
        );
        let navigation = self.call("Page.navigate", json!({"url":url}))?;
        if let Some(error) = navigation.get("errorText") {
            return Err(format!("navigation: {error}").into());
        }
        let expression = format!("(async()=>{{while(location.href!=={} || document.readyState!=='complete')await new Promise(r=>setTimeout(r,10));await document.fonts.ready;await Promise.all(Array.from(document.images,i=>i.decode()));await new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));return {{width:innerWidth,height:innerHeight,scale:devicePixelRatio,fonts:Array.from(document.fonts,f=>({{family:f.family,status:f.status}}))}}}})()", serde_json::to_string(&url)?);
        let ready = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
        )?;
        if let Some(error) = ready.get("exceptionDetails") {
            return Err(format!("page readiness: {error}").into());
        }
        let viewport = &ready["result"]["value"];
        if viewport["width"] != width || viewport["height"] != height || viewport["scale"] != 1 {
            return Err(format!("unexpected Chrome viewport: {viewport}").into());
        }
        let version = self.call("Browser.getVersion", json!({}))?;
        let layout = self.call("Page.getLayoutMetrics", json!({}))?;
        let elements = self.call("Runtime.evaluate", json!({"expression":"Array.from(document.querySelectorAll('section,.fixed'),e=>({tag:e.tagName,class:e.className,bounds:e.getBoundingClientRect().toJSON(),background:getComputedStyle(e).backgroundColor,position:getComputedStyle(e).position}))","returnByValue":true}))?;
        let capture = self.call(
            "Page.captureScreenshot",
            json!({"format":"png","captureBeyondViewport":false,"fromSurface":true,"clip":{"x":0,"y":0,"width":width,"height":height,"scale":1}}),
        )?;
        let data = capture["data"].as_str().ok_or("missing screenshot")?;
        let png = lumen_common::codec::base64_decode_forgiving(data.as_bytes())
            .ok_or("invalid screenshot base64")?;
        std::fs::write(screenshot, png)?;
        let hash = lumen_common::hash::digest(
            lumen_common::hash::Algo::Sha256,
            lumen_html_text::TEST_FONT_BYTES,
        );
        let html = std::fs::read_to_string(fixture)?;
        let directives: Vec<_> = html
            .lines()
            .filter(|line| line.contains("jixr-compare"))
            .collect();
        let metadata = json!({"chrome":version,"viewport":viewport,"layout":layout,"elements":elements["result"]["value"],"lumen_test_font_sha256":lumen_common::codec::hex_encode(&hash),"jixr_compare_directives":directives});
        std::fs::write(
            screenshot.with_extension("json"),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        self.session = None;
        self.call("Target.closeTarget", json!({"targetId":target}))?;
        Ok(())
    }
}
