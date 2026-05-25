use wasm_bindgen::prelude::*;
use std::io::Cursor;
use crate::zip::{process_new, Options};

#[wasm_bindgen]
pub struct RepairResult {
    data: Vec<u8>,
    log: String,
}

#[wasm_bindgen]
impl RepairResult {
    #[wasm_bindgen(getter)]
    pub fn data(&self) -> Vec<u8> {
        self.data.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn log(&self) -> String {
        self.log.clone()
    }
}

#[wasm_bindgen]
pub fn repair_zip(
    zip_data: &[u8],
    dry_run: bool,
    not_utf8: bool,
    keep_backslashes: bool,
) -> Result<RepairResult, JsValue> {
    let mut input = Cursor::new(zip_data.to_vec());
    let file_len = zip_data.len() as u64;
    let mut output = Cursor::new(Vec::new());
    let mut log_buf = Vec::new();

    let opts = Options {
        dry_run,
        fast: false, // not supported in WASM
        not_utf8,
        keep_backslashes,
        no_default_exclude: false,
        extra_excludes: Vec::new(),
    };

    match process_new(&mut input, file_len, &mut output, &opts, &mut log_buf) {
        Ok(_) => {
            let log = String::from_utf8_lossy(&log_buf).into_owned();
            Ok(RepairResult {
                data: output.into_inner(),
                log,
            })
        }
        Err(e) => Err(JsValue::from_str(&e.to_string())),
    }
}
