//! cargo run --release -- <slips dir> <models dir> <out dir>
//!
//! Renders five synthetic LabNorte pages at 200 dpi, degrades them (rotation, blur, noise, JPEG; the
//! "photo" ones harder, with uneven light), and reads them with each engine. Ground truth is the
//! page's own text layer. Prints one JSON line per engine and page, then a summary per engine.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use image::{ImageBuffer, Luma, Rgb, RgbImage};
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};
use pdfium_render::prelude::*;
use rand::{Rng, SeedableRng};

const PAGES: [(&str, i32, &str); 5] = [
    ("es-labnorte-3p", 1, "scan"),
    ("es-labnorte-3p", 2, "scan"),
    ("es-labnorte-3p", 3, "photo"),
    ("es-01-basic", 1, "photo"),
    ("es-04-wrapped", 1, "scan"),
];

fn pdfium() -> Result<Pdfium> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium/mac-arm64/lib");
    let lib = std::env::var("OWNHEALTH_PDFIUM_LIB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Pdfium::pdfium_platform_library_name_at_path(&dir));
    Ok(Pdfium::new(Pdfium::bind_to_library(&lib)?))
}

fn degrade(img: &RgbImage, kind: &str, seed: u64) -> Result<RgbImage> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let gray: ImageBuffer<Luma<u8>, Vec<u8>> = image::imageops::grayscale(img);
    let angle: f32 = if kind == "photo" { 3.5 } else { 1.2 } * if rng.random_bool(0.5) { 1.0 } else { -1.0 };
    let rotated = rotate_about_center(&gray, angle.to_radians(), Interpolation::Bilinear, imageproc::geometric_transformations::Border::Constant(Luma([255u8])));
    let sigma = if kind == "photo" { 1.1 } else { 0.7 };
    let blurred = imageproc::filter::gaussian_blur_f32(&rotated, sigma);
    let (w, h) = blurred.dimensions();
    let noise = if kind == "photo" { 14.0 } else { 8.0 };
    let mut out = RgbImage::new(w, h);
    for (x, y, p) in blurred.enumerate_pixels() {
        let mut v = p[0] as f64 + (rng.random::<f64>() - 0.5) * 2.0 * noise;
        if kind == "photo" {
            // uneven light: darker towards one corner, a warm cast
            v *= 0.78 + 0.22 * (1.0 - (x as f64 / w as f64 + y as f64 / h as f64) / 2.0);
        }
        let v = v.clamp(0.0, 255.0) as u8;
        let r = if kind == "photo" { v.saturating_add(6) } else { v };
        out.put_pixel(x, y, Rgb([r, v, v.saturating_sub(4)]));
    }
    let quality = if kind == "photo" { 55 } else { 75 };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&out)?;
    Ok(image::load_from_memory(&jpeg)?.to_rgb8())
}

fn tokens(text: &str) -> Vec<String> {
    text.split_whitespace().map(|t| t.trim_matches(|c: char| c == ',' && false).to_string()).collect()
}

#[derive(Default, Clone)]
struct Score {
    gt: usize,
    found: usize,
    out: usize,
    gt_accented: usize,
    found_accented: usize,
    gt_numeric: usize,
    found_numeric: usize,
}

fn score(gt: &[String], ocr: &[String]) -> Score {
    let mut bag: HashMap<&str, usize> = HashMap::new();
    for t in ocr {
        *bag.entry(t.as_str()).or_default() += 1;
    }
    let mut s = Score { gt: gt.len(), out: ocr.len(), ..Default::default() };
    for t in gt {
        let accented = t.chars().any(|c| !c.is_ascii());
        let numeric = t.chars().any(|c| c.is_ascii_digit());
        s.gt_accented += accented as usize;
        s.gt_numeric += numeric as usize;
        if let Some(n) = bag.get_mut(t.as_str()).filter(|n| **n > 0) {
            *n -= 1;
            s.found += 1;
            s.found_accented += accented as usize;
            s.found_numeric += numeric as usize;
        }
    }
    s
}

trait Engine {
    fn name(&self) -> String;
    fn read(&mut self, img: &RgbImage) -> Result<String>;
}

struct Oar(String, oar_ocr::oarocr::OAROCR);
impl Engine for Oar {
    fn name(&self) -> String {
        self.0.clone()
    }
    fn read(&mut self, img: &RgbImage) -> Result<String> {
        let res = self.1.predict(vec![img.clone()]).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut lines = Vec::new();
        for r in &res[0].text_regions {
            if let Some((t, _)) = r.text_with_confidence() {
                lines.push(t.to_string());
            }
        }
        Ok(lines.join("\n"))
    }
}

struct Ocrs(ocrs::OcrEngine);
impl Engine for Ocrs {
    fn name(&self) -> String {
        "ocrs-0.14".into()
    }
    fn read(&mut self, img: &RgbImage) -> Result<String> {
        let src = ocrs::ImageSource::from_bytes(img.as_raw(), img.dimensions())?;
        let input = self.0.prepare_input(src)?;
        Ok(self.0.get_text(&input)?)
    }
}

fn oar(models: &Path, label: &str, det: &str, rec: &str, dict: &str) -> Result<(Box<dyn Engine>, f64)> {
    let t = Instant::now();
    let e = oar_ocr::oarocr::OAROCRBuilder::new(
        models.join(det).to_string_lossy().to_string(),
        models.join(rec).to_string_lossy().to_string(),
        models.join(dict).to_string_lossy().to_string(),
    )
    .build()
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((Box::new(Oar(label.into(), e)), t.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let slips = PathBuf::from(args.first().context("slips dir")?);
    let models = PathBuf::from(args.get(1).context("models dir")?);
    let out = PathBuf::from(args.get(2).context("out dir")?);
    std::fs::create_dir_all(&out)?;
    let only: Option<&String> = args.get(3);

    let pdfium = pdfium()?;
    let mut pages = Vec::new();
    for (i, (slip, page_no, kind)) in PAGES.iter().enumerate() {
        let doc = pdfium.load_pdf_from_file(&slips.join(format!("{slip}.pdf")), None)?;
        let page = doc.pages().get(page_no - 1)?;
        let gt = tokens(&page.text()?.all());
        let cfg = PdfRenderConfig::new().scale_page_by_factor(200.0 / 72.0);
        let img = page.render_with_config(&cfg)?.as_image()?.to_rgb8();
        let deg = degrade(&img, kind, 7 + i as u64)?;
        let name = format!("{slip}-p{page_no}-{kind}");
        deg.save(out.join(format!("{name}.png")))?;
        pages.push((name, gt, deg));
    }

    let mut engines: Vec<(Box<dyn Engine>, f64)> = Vec::new();
    let want = |n: &str| only.is_none_or(|o| o == n);
    if want("v5-mobile-latin") {
        engines.push(oar(&models, "ppocrv5-mobile-det+latin-mobile-rec", "pp-ocrv5_mobile_det.onnx", "latin_pp-ocrv5_mobile_rec.onnx", "ppocrv5_latin_dict.txt")?);
    }
    if want("v5-server") {
        engines.push(oar(&models, "ppocrv5-server-det+server-rec", "pp-ocrv5_server_det.onnx", "pp-ocrv5_server_rec.onnx", "ppocrv5_dict.txt")?);
    }
    if want("v6-small") {
        engines.push(oar(&models, "ppocrv6-small-det+small-rec", "pp-ocrv6_small_det.onnx", "pp-ocrv6_small_rec.onnx", "ppocrv6_dict.txt")?);
    }
    if want("ocrs") {
        let t = Instant::now();
        let e = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
            detection_model: Some(rten::Model::load_file(models.join("text-detection.onnx"))?),
            recognition_model: Some(rten::Model::load_file(models.join("text-recognition.onnx"))?),
            ..Default::default()
        })?;
        engines.push((Box::new(Ocrs(e)), t.elapsed().as_secs_f64()));
    }

    for (engine, load_s) in engines.iter_mut() {
        let mut total = Score::default();
        let mut secs = Vec::new();
        for (name, gt, img) in &pages {
            let t = Instant::now();
            let text = engine.read(img)?;
            let s_page = t.elapsed().as_secs_f64();
            secs.push(s_page);
            std::fs::write(out.join(format!("{name}.{}.txt", engine.name())), &text)?;
            let s = score(gt, &tokens(&text));
            println!(
                "{}",
                serde_json::json!({"engine": engine.name(), "page": name, "seconds": s_page,
                    "word_recall": s.found as f64 / s.gt as f64, "word_precision": s.found as f64 / s.out.max(1) as f64,
                    "accented_recall": s.found_accented as f64 / s.gt_accented.max(1) as f64,
                    "numeric_recall": s.found_numeric as f64 / s.gt_numeric.max(1) as f64, "gt_words": s.gt})
            );
            total.gt += s.gt;
            total.found += s.found;
            total.out += s.out;
            total.gt_accented += s.gt_accented;
            total.found_accented += s.found_accented;
            total.gt_numeric += s.gt_numeric;
            total.found_numeric += s.found_numeric;
        }
        let mean = secs.iter().sum::<f64>() / secs.len() as f64;
        println!(
            "SUMMARY {}",
            serde_json::json!({"engine": engine.name(), "load_seconds": load_s, "mean_seconds_per_page": mean,
                "max_seconds_per_page": secs.iter().cloned().fold(0.0, f64::max),
                "word_recall": total.found as f64 / total.gt as f64,
                "word_precision": total.found as f64 / total.out.max(1) as f64,
                "accented_recall": total.found_accented as f64 / total.gt_accented.max(1) as f64,
                "numeric_recall": total.found_numeric as f64 / total.gt_numeric.max(1) as f64,
                "gt_words": total.gt, "gt_accented": total.gt_accented, "gt_numeric": total.gt_numeric})
        );
    }
    Ok(())
}
