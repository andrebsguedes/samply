use crate::{api_file_path::to_api_file_path, to_debug_id};
use samply_symbols::{
    FileAndPathHelper, FileAndPathHelperError, LibraryInfo, LookupAddress, SymbolManager,
};
use serde_json::json;
use serde_derive::Deserialize;

mod request_json;
mod response_json;

#[derive(thiserror::Error, Debug)]
enum SourceError {
    #[error("Couldn't parse request: {0}")]
    ParseRequestErrorSerde(#[from] serde_json::error::Error),

    #[error("Could not obtain symbols for the requested library: {0}")]
    NoSymbols(#[from] samply_symbols::Error),

    #[error("Don't have any debug info for the requested library")]
    NoDebugInfo,

    #[error("The requested path is not present in the symbolication frames")]
    InvalidPath,

    #[error("An error occurred when reading the file: {0}")]
    FileAndPathHelperError(#[from] FileAndPathHelperError),
}

pub struct SourceApi<'a, H: FileAndPathHelper> {
    symbol_manager: &'a SymbolManager<H>,
}

impl<'a, H: FileAndPathHelper> SourceApi<'a, H> {
    /// Create a [`SourceApi`] instance which uses the provided [`SymbolManager`].
    pub fn new(symbol_manager: &'a SymbolManager<H>) -> Self {
        Self { symbol_manager }
    }

    pub async fn query_api_json(&self, request_json: &str) -> String {
        match self.query_api_fallible_json(request_json).await {
            Ok(response_json) => response_json,
            Err(err) => json!({ "error": err.to_string() }).to_string(),
        }
    }

    async fn query_api_fallible_json(&self, request_json: &str) -> Result<String, SourceError> {
        let request: request_json::Request = serde_json::from_str(request_json)?;
        let response = self.query_api(&request).await?;
        Ok(serde_json::to_string(&response)?)
    }

    async fn query_api(
        &self,
        request: &request_json::Request,
    ) -> Result<response_json::Response, SourceError> {
        let request_json::Request {
            debug_id,
            debug_name,
            module_offset,
            file: requested_file,
        } = &request;
        let debug_id = to_debug_id(debug_id)?;

        // Look up the address to see which file paths we are allowed to read.
        let info = LibraryInfo {
            debug_name: Some(debug_name.to_string()),
            debug_id: Some(debug_id),
            ..Default::default()
        };
        let symbol_map = self.symbol_manager.load_symbol_map(&info).await?;
        let debug_file_location = symbol_map.debug_file_location().clone();
        let address_info = symbol_map
            .lookup(LookupAddress::Relative(*module_offset))
            .await;
        let frames = address_info
            .and_then(|ai| ai.frames)
            .ok_or(SourceError::NoDebugInfo)?;

        // Find the SourceFilePath whose "api file path" matches the requested file.
        // This is where we check that the requested file path is permissible.
        let source_file_path = frames
            .into_iter()
            .filter_map(|frame| frame.file_path)
            .find(|file_path| to_api_file_path(file_path) == *requested_file)
            .ok_or(SourceError::InvalidPath)?;

        let paths = expand_path(&source_file_path);
        let mut result = None;
        for path in paths {
            // If we got here, it means that the file access is allowed. Read the file.
            let res = self
                .symbol_manager
                .load_source_file(&debug_file_location, &path)
                .await;

            if res.is_ok() {
                result = Some(res);
                break;
            } else if result.is_none() {
                result = Some(res);
            }
        }

        match result {
            Some(Ok(source)) => {
                Ok(response_json::Response {
                    symbols_last_modified: None,
                    source_last_modified: None,
                    file: requested_file.to_string(),
                    source,
                })
            }
            Some(Err(e)) => Err(e.into()),
            _ => unreachable!("")
        }

    }
}

#[derive(Deserialize, Debug)]
struct SamplyConfig {
    search_paths: Vec<String>,
    replace_patterns: std::collections::HashMap<String, String>,
}

fn expand_path(s: &samply_symbols::SourceFilePath) -> Vec<samply_symbols::SourceFilePath> {
    use std::path::Path;
    use regex::Regex;

    let samply_config: Option<SamplyConfig> = std::fs::read_to_string("./samply.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    if let Some(config) = samply_config {
        let path = Path::new(s.raw_path());
        let mut result_paths = vec![path.to_owned()];
        for (pattern, replacement) in config.replace_patterns {
            if let Ok(re) = Regex::new(&pattern) {
                let path_str = re.replace(s.raw_path(), replacement);
                if let Ok(resolved_path) = Path::new(path_str.as_ref()).canonicalize() {
                    result_paths.push(resolved_path);
                }
            }
        }
        if path.is_relative() {
            for search_path in config.search_paths {
                if let Ok(resolved_path) = Path::new(&search_path).join(path).canonicalize() {
                    result_paths.push(resolved_path);
                }
            }
        }
        result_paths
            .into_iter()
            .map(|p| samply_symbols::SourceFilePath::new(p.to_string_lossy().to_string(), None))
            .collect()
    } else {
        vec![s.clone()]
    }
}
