use anyhow::Result;
use std::path::Path;

/// Extract text from a PDF.
///
/// `pdf_extract` panics on malformed or exotic PDFs (e.g.
/// "unexpected entry in unicode map", observed crashing the read tool with
/// "Tool task panicked"). Contain those panics here and surface them as
/// ordinary errors so a bad PDF cannot take down the calling tool task.
pub fn extract_text(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    match std::panic::catch_unwind(move || pdf_extract::extract_text(&path)) {
        Ok(result) => Ok(result?),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(anyhow::anyhow!(
                "PDF text extraction failed (parser panic: {message}). The file may be malformed or use unsupported encoding."
            ))
        }
    }
}
