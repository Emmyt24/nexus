//! Print the generated OpenAPI document to stdout.
//!
//! The contract for this API is generated from the `#[utoipa::path]` attributes
//! and served at `/api/openapi.json`; no spec file is checked in. That makes it
//! invisible in a diff and impossible to inspect without a running server and a
//! database — which is how four annotated routes once went unregistered
//! (`b3eca25`) and two referenced a security scheme that did not exist
//! (`e2f913b`).
//!
//! This needs neither, because the document comes from the macros:
//!
//! ```text
//! cargo run --bin openapi > docs/openapi.json
//! cargo run --bin openapi | jq '.paths | keys | length'
//! ```

fn main() {
    let spec = nexuscare_backend::routes::app_routes::openapi_spec();
    match serde_json::to_string_pretty(&spec) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("could not serialise the OpenAPI document: {e}");
            std::process::exit(1);
        }
    }
}
