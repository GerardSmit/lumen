//! Run fresh Chrome references and save both renders, absolute pixel diffs, and CSV metrics.
use lumen_html_image::{decode_png, encode_png, render_html_with_images, FileImages, Rgba8Image};
use lumen_html_text::{FontFace, TEST_FONT_BYTES};
use std::{path::PathBuf, sync::Arc};

#[path = "support/chrome.rs"]
mod chrome;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let browser = PathBuf::from(
        args.next()
            .ok_or("usage: compare CHROMIUM OUTPUT_DIRECTORY [CASES_DIRECTORY WIDTHxHEIGHT]")?,
    )
    .canonicalize()?;
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    std::fs::create_dir_all(&output)?;
    let output = output.canonicalize()?;
    let fixtures = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/render"))
        .canonicalize()?;
    let size = args.next().unwrap_or_else(|| "64x64".into());
    let size = size.to_str().ok_or("invalid viewport")?;
    let (width, height) = size
        .split_once('x')
        .ok_or("viewport must be WIDTHxHEIGHT")?;
    let (width, height): (u32, u32) = (width.parse()?, height.parse()?);
    if width == 0 || height == 0 || args.next().is_some() {
        return Err("invalid comparison arguments".into());
    }
    let mut chrome = chrome::Chrome::launch(&browser, &output)?;
    let mut cdp = chrome.connect()?;
    let font = FontFace::new(Arc::from(TEST_FONT_BYTES))?;
    let mut metrics = String::from(
        "fixture,differing_pixels,total_pixels,max_channel_delta,mean_channel_delta\n",
    );
    let mut errors = Vec::new();
    let mut names = std::fs::read_dir(&fixtures)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    names.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "html")
    });
    names.sort();
    for fixture in names {
        let name = fixture
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or("invalid fixture name")?;
        let screenshot = output.join(format!("{name}.png"));
        if let Err(error) = cdp.capture(&fixture.canonicalize()?, &screenshot, width, height) {
            eprintln!("{name}: Chrome: {error}");
            errors.push(
                serde_json::json!({"fixture":name,"stage":"chrome","error":error.to_string()}),
            );
            continue;
        }
        let html = std::fs::read_to_string(fixtures.join(format!("{name}.html")))?;
        let actual = match render_html_with_images(
            &html,
            width,
            height,
            1.0,
            &font,
            &FileImages::new(&fixtures),
        ) {
            Ok(actual) => actual,
            Err(error) => {
                eprintln!("{name}: Lumen: {error:?}");
                errors.push(serde_json::json!({"fixture":name,"stage":"lumen","error":format!("{error:?}")}));
                continue;
            }
        };
        let reference = decode_png(&std::fs::read(output.join(format!("{name}.png")))?)
            .map_err(|error| format!("{name} reference: {error:?}"))?;
        if (actual.width, actual.height) != (reference.width, reference.height) {
            return Err(format!("{name}: reference dimensions differ").into());
        }
        std::fs::write(
            output.join(format!("{name}-lumen.png")),
            encode_png(&actual),
        )?;
        let mut diff = Rgba8Image {
            width: actual.width,
            height: actual.height,
            pixels: vec![0; actual.pixels.len()],
        };
        let mut changed = 0usize;
        let mut maximum = 0u8;
        let mut sum = 0u64;
        for ((a, b), d) in actual
            .pixels
            .chunks_exact(4)
            .zip(reference.pixels.chunks_exact(4))
            .zip(diff.pixels.chunks_exact_mut(4))
        {
            changed += usize::from(a != b);
            for channel in 0..4 {
                let delta = a[channel].abs_diff(b[channel]);
                maximum = maximum.max(delta);
                sum += u64::from(delta);
                d[channel] = delta;
            }
            d[3] = 255;
        }
        std::fs::write(output.join(format!("{name}-diff.png")), encode_png(&diff))?;
        let row = format!(
            "{name},{changed},{},{maximum},{:.6}\n",
            actual.pixels.len() / 4,
            sum as f64 / actual.pixels.len() as f64
        );
        print!("{row}");
        metrics.push_str(&row);
        if name == "layers" && (width, height) == (64, 64) {
            for (x, y) in [(5, 5), (20, 25), (5, 40), (25, 40)] {
                let offset = (y * 64 + x) * 4;
                println!(
                    "layers ({x},{y}): Lumen {:?}, Chrome {:?}",
                    &actual.pixels[offset..offset + 4],
                    &reference.pixels[offset..offset + 4]
                );
            }
        }
        if name == "paint" && (width, height) == (64, 64) {
            for (label, rows) in [("rounded", 0..24), ("PNG scaling", 24..64)] {
                let mut changed = 0;
                let mut maximum = 0;
                for y in rows {
                    for x in 0..64 {
                        let offset = (y * 64 + x) * 4;
                        let a = &actual.pixels[offset..offset + 4];
                        let b = &reference.pixels[offset..offset + 4];
                        changed += usize::from(a != b);
                        maximum = maximum
                            .max(a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)).max().unwrap());
                    }
                }
                println!("paint {label}: {changed} differing pixels, max delta {maximum}");
            }
        }
        if name == "text" && (width, height) == (64, 64) {
            for line in 0..3 {
                let mut bounds = [(64usize, 64usize, 0usize, 0usize); 2];
                let mut unmatched = [0usize; 2];
                let images = [&actual, &reference];
                for side in 0..2 {
                    for y in line * 16..(line + 1) * 16 {
                        for x in 0..64 {
                            let ink = |image: &Rgba8Image, x: usize, y: usize| {
                                let p = &image.pixels[(y * 64 + x) * 4..][..3];
                                p.iter().map(|v| u32::from(*v)).sum::<u32>() < 480
                            };
                            if !ink(images[side], x, y) {
                                continue;
                            }
                            let b = &mut bounds[side];
                            b.0 = b.0.min(x);
                            b.1 = b.1.min(y);
                            b.2 = b.2.max(x);
                            b.3 = b.3.max(y);
                            if !(y.saturating_sub(1)..=(y + 1).min(63)).any(|ny| {
                                (x.saturating_sub(1)..=(x + 1).min(63))
                                    .any(|nx| ink(images[1 - side], nx, ny))
                            }) {
                                unmatched[side] += 1;
                            }
                        }
                    }
                }
                println!("text line {}: ink bounds Lumen {:?}, Chrome {:?}; unmatched ink beyond 1px {:?}", line+1, bounds[0], bounds[1], unmatched);
            }
        }
    }
    std::fs::write(output.join("metrics.csv"), metrics)?;
    std::fs::write(
        output.join("errors.json"),
        serde_json::to_vec_pretty(&errors)?,
    )?;
    drop(cdp);
    drop(chrome);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} fixtures failed; see errors.json", errors.len()).into())
    }
}
