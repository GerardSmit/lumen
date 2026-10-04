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
        self.call("Target.activateTarget", json!({"targetId":target}))?;
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
        let path = path.trim_start_matches('/');
        let url = format!(
            "file:///{}",
            lumen_common::codec::percent_encode(path.as_bytes(), |c| !c.is_ascii_alphanumeric()
                && !b"/:.-_~".contains(&c))
        );
        let navigation = self.call("Page.navigate", json!({"url":url}))?;
        if let Some(error) = navigation.get("errorText") {
            return Err(format!("navigation: {error}").into());
        }
        let expression = r#"(async()=>{
          while(location.href!==__TARGET__ || document.readyState!=='complete')
            await new Promise(r=>setTimeout(r,10));

          const regular=__REGULAR__, bold=__BOLD__, mono=__MONO__;
          const toFontUrl=encoded=>{
            const binary=atob(encoded), bytes=new Uint8Array(binary.length);
            for(let i=0;i<binary.length;i++) bytes[i]=binary.charCodeAt(i);
            return URL.createObjectURL(new Blob([bytes],{type:'font/ttf'}));
          };
          const urls={regular:toFontUrl(regular),bold:toFontUrl(bold),mono:toFontUrl(mono)};
          const faces=[];
          for(const family of ['LumenTest','Arial','Helvetica','Test','Liberation Sans','sans-serif']) {
            faces.push([family,urls.regular,400],[family,urls.bold,700]);
          }
          for(const family of ['LumenMono','monospace','Inconsolata'])
            faces.push([family,urls.mono,400]);
          const fontCss=faces.map(([family,src,weight])=>
            `@font-face{font-family:${JSON.stringify(family)};src:url("${src}") format("truetype");font-weight:${weight};font-style:normal}`
          ).join('\n')+'\nhtml{font-family:"LumenTest"}';
          const injected=document.createElement('style');
          injected.dataset.lumenComparisonFonts='true';
          injected.textContent=fontCss;
          const head=document.head||document.documentElement;
          head.insertBefore(injected,head.firstChild);

          const familyParts=value=>{
            const parts=[];let start=0,quote='',depth=0,escaped=false;
            for(let i=0;i<value.length;i++){
              const c=value[i];
              if(escaped){escaped=false;continue;}
              if(c==='\\'){escaped=true;continue;}
              if(quote){if(c===quote)quote='';continue;}
              if(c==='"'||c==="'"){quote=c;continue;}
              if(c==='('){depth++;continue;}
              if(c===')'&&depth){depth--;continue;}
              if(c===','&&!depth){parts.push(value.slice(start,i));start=i+1;}
            }
            parts.push(value.slice(start));
            return parts;
          };
          const decodeIdentifier=value=>value.replace(/\\([0-9a-fA-F]{1,6})(?:\r\n|[ \t\r\n\f])?|\\([^\r\n])/g,
            (_match,hex,escaped)=>{
              if(!hex)return escaped;
              const codepoint=parseInt(hex,16);
              return !codepoint||codepoint>0x10ffff||(codepoint>=0xd800&&codepoint<=0xdfff)
                ?'\ufffd':String.fromCodePoint(codepoint);
            });
          const familyName=part=>{
            const value=part.trim();
            if((value[0]==='"'&&value.at(-1)==='"')||(value[0]==="'"&&value.at(-1)==="'"))
              return decodeIdentifier(value.slice(1,-1));
            return decodeIdentifier(value);
          };
          const injectedFamilies=new Set(faces.map(face=>face[0].toLowerCase()));
          const authoredFaces=new Set(),seenFamilies=new Set(),inaccessible=[];
          const familyRewrite=value=>familyParts(value).map(part=>{
            const name=familyName(part), key=name.toLowerCase();
            if(name)seenFamilies.add(name);
            const trimmed=part.trim();
            const unquoted=trimmed[0]!=='"'&&trimmed[0]!=="'"&&!/\s/.test(trimmed);
            if(unquoted){
              const leading=part.match(/^\s*/)[0],trailing=part.match(/\s*$/)[0];
              if(key==='sans-serif')return leading+'LumenTest'+trailing;
              if(key==='monospace')return leading+'LumenMono'+trailing;
            }
            return part;
          }).join(',');
          const rewriteStyle=style=>{
            const value=style.getPropertyValue('font-family');
            if(!value)return;
            const rewritten=familyRewrite(value);
            if(rewritten!==value){
              const priority=style.getPropertyPriority('font-family')||style.getPropertyPriority('font');
              style.setProperty('font-family',rewritten,priority);
            }
          };
          const scanRule=rule=>{
            if(rule.type===5&&rule.style){
              const family=rule.style.getPropertyValue('font-family');
              if(family)authoredFaces.add(familyName(family).toLowerCase());
            }else if(rule.type===1&&rule.style){
              rewriteStyle(rule.style);
            }
            if(rule.cssRules)for(const child of rule.cssRules)scanRule(child);
          };
          for(const sheet of document.styleSheets){
            if(sheet.ownerNode===injected)continue;
            try{for(const rule of sheet.cssRules)scanRule(rule);}
            catch(_error){inaccessible.push(sheet.href||'<inline stylesheet>');}
          }
          for(const element of document.querySelectorAll('[style]'))rewriteStyle(element.style);
          const pinnedFamilies=['LumenTest','Arial','Helvetica','Test','Liberation Sans','sans-serif','LumenMono','monospace','Inconsolata'];
          const unpinned=[...seenFamilies].filter(name=>{
            const key=name.toLowerCase();
            return !injectedFamilies.has(key)&&!authoredFaces.has(key)&&key!=='lumentest'&&key!=='lumenmono';
          }).sort((a,b)=>a.localeCompare(b));
          await Promise.all([
            ...faces.map(([family,_src,weight])=>
              document.fonts.load(`${weight} 12px ${JSON.stringify(family)}`)
            ),
            document.fonts.ready,
            Promise.all(Array.from(document.images,i=>i.decode()))
          ]);
          await new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));
          return {
            width:innerWidth,height:innerHeight,scale:devicePixelRatio,
            fonts:Array.from(document.fonts,f=>({family:f.family,status:f.status})),
            font_pinning:{
              default_family:'LumenTest',
              family_mapping:{'sans-serif':'LumenTest','monospace':'LumenMono'},
              injected_aliases:pinnedFamilies,
              authored_font_faces:[...authoredFaces].sort(),
              unpinned_authored_families:unpinned,
              inaccessible_stylesheets:inaccessible
            }
          };
        })()"#
            .replace("__TARGET__", &serde_json::to_string(&url)?)
            .replace(
                "__REGULAR__",
                &serde_json::to_string(&lumen_common::codec::base64_encode(
                    lumen_html_text::TEST_FONT_BYTES,
                    false,
                    true,
                ))?,
            )
            .replace(
                "__BOLD__",
                &serde_json::to_string(&lumen_common::codec::base64_encode(
                    lumen_html_text::TEST_FONT_BOLD_BYTES,
                    false,
                    true,
                ))?,
            )
            .replace(
                "__MONO__",
                &serde_json::to_string(&lumen_common::codec::base64_encode(
                    lumen_html_text::DEFAULT_FONT_BYTES,
                    false,
                    true,
                ))?,
            );
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
        let features=self.call("Runtime.evaluate",json!({"expression":"({crossFade:CSS.supports('background-image','cross-fade(red 50%,blue 50%)'),lightDarkImage:CSS.supports('background-image','light-dark(image(red),image(blue))'),singleStopGradient:CSS.supports('background-image','linear-gradient(red)'),borderAreaClip:CSS.supports('background-clip','border-area')})","returnByValue":true}))?;
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
        let bold_hash = lumen_common::hash::digest(
            lumen_common::hash::Algo::Sha256,
            lumen_html_text::TEST_FONT_BOLD_BYTES,
        );
        let mono_hash = lumen_common::hash::digest(
            lumen_common::hash::Algo::Sha256,
            lumen_html_text::DEFAULT_FONT_BYTES,
        );
        let html = std::fs::read_to_string(fixture)?;
        let directives: Vec<_> = html
            .lines()
            .filter(|line| line.contains("jixr-compare"))
            .collect();
        let metadata = json!({"chrome":version,"viewport":viewport,"layout":layout,"elements":elements["result"]["value"],"css_feature_support":features["result"]["value"],"lumen_test_font_sha256":lumen_common::codec::hex_encode(&hash),"lumen_test_bold_font_sha256":lumen_common::codec::hex_encode(&bold_hash),"lumen_mono_font_sha256":lumen_common::codec::hex_encode(&mono_hash),"font_pinning":viewport["font_pinning"],"jixr_compare_directives":directives});
        std::fs::write(
            screenshot.with_extension("json"),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        self.session = None;
        self.call("Target.closeTarget", json!({"targetId":target}))?;
        Ok(())
    }
}
