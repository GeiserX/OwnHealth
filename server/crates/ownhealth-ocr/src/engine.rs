//! The OCR engine (lane C). `load` reads the models once per process from `OWNHEALTH_OCR_MODELS_DIR`
//! (checking the files `scripts/fetch-ocr-models.sh` wrote); `read_page` is CPU work: call it from
//! `spawn_blocking`.

use std::path::Path;

use crate::{OcrError, OcrPage};

/// The model files, as `scripts/fetch-ocr-models.sh` names them.
pub const DET_MODEL: &str = "pp-ocrv5_mobile_det.onnx";
pub const REC_MODEL: &str = "latin_pp-ocrv5_mobile_rec.onnx";
pub const REC_DICT: &str = "ppocrv5_latin_dict.txt";

/// A loaded engine. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct Engine {
    _private: (),
}

/// Load the models from `dir`. Without the `engine` feature: `OcrError::EngineMissing`.
pub fn load(_dir: &Path) -> Result<Engine, OcrError> {
    Err(OcrError::EngineMissing)
}

/// Read one page image (RGB8, row-major, `width * height * 3` bytes) rendered at `dpi`; word boxes come
/// back in PDF points.
pub fn read_page(
    _engine: &Engine,
    _rgb: &[u8],
    _width: u32,
    _height: u32,
    _dpi: f64,
    _page_no: i32,
) -> Result<OcrPage, OcrError> {
    Err(OcrError::EngineMissing)
}
