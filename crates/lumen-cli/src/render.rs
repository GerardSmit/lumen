//! HTML rendering with optional DOM mutation before deterministic image output.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut font = None;
    let mut script = None;
    let mut entry = None;
    let mut size = (800u32, 600u32);
    let mut scale = 1.0f32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--html" => {
                i += 1;
                input = Some(args.get(i).ok_or("--html needs a path")?.as_str());
            }
            "--size" => {
                i += 1;
                let raw = args.get(i).ok_or("--size needs WIDTHxHEIGHT")?;
                let (width, height) = raw.split_once('x').ok_or("--size needs WIDTHxHEIGHT")?;
                size = (
                    width.parse().map_err(|_| "invalid width")?,
                    height.parse().map_err(|_| "invalid height")?,
                );
            }
            "--scale" => {
                i += 1;
                scale = args
                    .get(i)
                    .ok_or("--scale needs a number")?
                    .parse()
                    .map_err(|_| "invalid scale")?;
            }
            "--font" => {
                i += 1;
                font = Some(args.get(i).ok_or("--font needs a path")?.as_str());
            }
            "--script" => {
                i += 1;
                script = Some(args.get(i).ok_or("--script needs a path")?.as_str());
            }
            "-o" | "--output" => {
                i += 1;
                output = Some(args.get(i).ok_or("-o needs a path")?.as_str());
            }
            value if !value.starts_with('-') && entry.is_none() => entry = Some(value),
            _ => return Err(format!("unknown render argument: {}", args[i])),
        }
        i += 1;
    }
    if let Some(path) = entry {
        if std::path::Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "html" || extension == "htm")
        {
            if input.replace(path).is_some() {
                return Err("render accepts one HTML file".into());
            }
        } else if script.replace(path).is_some() {
            return Err("render accepts one app script".into());
        }
    }
    if input.is_none() && script.is_none() {
        return Err("render needs an HTML file or app script".into());
    }
    if size.0 == 0 || size.1 == 0 || !scale.is_finite() || scale <= 0.0 {
        return Err("render needs a positive viewport and finite positive scale".into());
    }
    let output = output.ok_or("render needs -o OUT.png")?;
    let font_path = font.ok_or("render needs --font FONT.ttf")?;
    let source = match input {
        Some(input) => {
            std::fs::read_to_string(input).map_err(|error| format!("{input}: {error}"))?
        }
        None => "<html><head></head><body></body></html>".into(),
    };
    let font_bytes = std::fs::read(font_path).map_err(|error| format!("{font_path}: {error}"))?;
    let font = lumen_html_text::FontFace::new(std::sync::Arc::from(font_bytes))
        .map_err(|error| format!("{font_path}: {error}"))?;
    let assets = lumen_html_image::FileImages::new(
        std::path::Path::new(input.or(script).unwrap())
            .parent()
            .unwrap_or(std::path::Path::new(".")),
    );
    let image = if let Some(script_path) = script {
        let path = std::fs::canonicalize(script_path)
            .map_err(|error| format!("{script_path}: {error}"))?;
        let path = lumen_host::strip_verbatim(path)
            .to_string_lossy()
            .into_owned();
        let options = lumen_html_image::SettleOptions::default();
        let mut runtime = lumen_runtime::Runtime::new();
        runtime.set_deadline(options.timeout);
        runtime.engine().set_jsx_options(lumen::JsxOptions {
            runtime: lumen::JsxRuntime::Automatic,
            import_source: "lumen".into(),
            ..Default::default()
        });
        runtime.install_module_loader_with(&path, false, lumen_html_js::module_source);
        let realm = lumen_html_js::install(runtime.engine().ctx(), &source, 65_536)
            .map_err(|error| format!("HTML setup failed: {error:?}"))?;
        let script = format!("globalThis.__lumenRenderImport = null; import({}).then(()=>{{globalThis.__lumenRenderImport=true}},error=>{{globalThis.__lumenRenderImport=String(error)}})", serde_json::to_string(&path).unwrap());
        let result = runtime
            .engine()
            .eval_value(&script)
            .map_err(|error| format!("{script_path}:{}: {}", error.line, error.message))?;
        if result.is_err() {
            return Err(format!("{script_path}: script threw"));
        }
        let image = lumen_html_image::render_settled_shared(
            &realm.session_handle(),
            size.0,
            size.1,
            scale,
            &font,
            &assets,
            options,
            || !runtime.run_until_idle().idle,
        )
        .map_err(|error| format!("render failed: {error:?}"))?;
        if runtime.fatal_exit_code().is_some() {
            return Err(format!("{script_path}: app jobs failed"));
        }
        let engine = runtime.engine();
        let global = engine.global_this();
        match engine.ctx().get_member(&global, "__lumenRenderImport") {
            Ok(lumen::embed::Value::Bool(true)) if !runtime.is_interrupted() => image,
            Ok(lumen::embed::Value::Str(message)) => {
                return Err(format!("{script_path}: {message}"))
            }
            _ => return Err(format!("{script_path}: app did not settle")),
        }
    } else {
        lumen_html_image::render_html_with_images(&source, size.0, size.1, scale, &font, &assets)
            .map_err(|error| format!("render failed: {error:?}"))?
    };
    let png = lumen_html_image::encode_png(&image);
    std::fs::write(output, png).map_err(|error| format!("{output}: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tsx_imports_and_promise_dom_mutation_render_the_static_golden() {
        std::thread::Builder::new().stack_size(lumen::THREAD_STACK_SIZE).spawn(|| {
            let directory = std::env::temp_dir().join(format!("lumen-render-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            std::fs::create_dir(&directory).unwrap();
            let html = directory.join("index.html");
            let font = directory.join("font.ttf");
            let app = directory.join("app.tsx");
            let output = directory.join("output.png");
            std::fs::write(&html, "<body style='margin:0'><main id=app></main></body>").unwrap();
            std::fs::write(&font, lumen_html_text::TEST_FONT_BYTES).unwrap();
            std::fs::write(directory.join("color.ts"), "export const color: string = 'blue';").unwrap();
            std::fs::write(&app, "import {color} from './color.ts'; import {jsx} from 'lumen/jsx-runtime'; const node = <div style={{width:4,height:4,background:'red'}}/>; document.getElementById('app').appendChild(node); Promise.resolve().then(()=>Promise.resolve()).then(()=>{node.style.backgroundColor=color;});").unwrap();
            let args = vec![app.display().to_string(), "--html".into(), html.display().to_string(), "--font".into(), font.display().to_string(), "--size".into(), "8x8".into(), "--scale".into(), "2".into(), "-o".into(), output.display().to_string()];
            super::run(&args).unwrap();
            let actual = lumen_html_image::decode_png(&std::fs::read(&output).unwrap()).unwrap();
            let font = lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
            let expected = lumen_html_image::render_html_with_font("<body style='margin:0'><main><div style='width:4px;height:4px;background:blue'></div></main></body>", 8, 8, 2.0, &font).unwrap();
            assert!(actual == expected, "first actual {:?}, expected {:?}", &actual.pixels[..4], &expected.pixels[..4]);
            std::fs::write(&app, "import './missing.ts';").unwrap();
            assert!(super::run(&args).unwrap_err().contains("app.tsx"));
            std::fs::write(&app, "const broken = <div;").unwrap();
            assert!(super::run(&args).unwrap_err().contains("app.tsx"));
        }).unwrap().join().unwrap();
    }
}
