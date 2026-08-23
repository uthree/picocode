//! Checks `fetch_context_limit` against a real Ollama, which is the only
//! provider that can be reached without a key.
//!
//! Ignored by default: it needs a server on the default port with at least
//! one model pulled, so CI would fail on it for the wrong reason. Run it
//! deliberately when touching the parser or the endpoint:
//!
//! ```sh
//! cargo test -p picocode-core --test context_limit_live -- --ignored --nocapture
//! ```

use picocode_core::config::Provider;
use picocode_core::models;

#[tokio::test]
#[ignore = "needs a local Ollama with a model pulled"]
async fn ollama_reports_the_window_of_a_model_it_serves() {
    // The binaries do this in `Config::from_args`; reqwest refuses to
    // build a client without it.
    picocode_core::config::install_tls_provider();

    let served = models::fetch(Provider::Ollama, None)
        .await
        .expect("no Ollama on the default port");
    let model = served
        .first()
        .expect("Ollama serves no models — pull one first");

    let limit = models::fetch_context_limit(Provider::Ollama, None, model)
        .await
        .expect("the show request failed")
        .expect("Ollama reported no context length");
    println!("{model}: {limit} tokens");
    // Every architecture Ollama serves declares a window well past this;
    // the assertion is really that a plausible number came back rather
    // than a 0 or a byte count from the wrong field.
    assert!(limit >= 2048, "{model} reported {limit}");

    // A model the server does not have is an error from the server, not a
    // silent zero — the caller treats it as "don't know" either way.
    assert!(
        models::fetch_context_limit(Provider::Ollama, None, "no-such-model:0b")
            .await
            .is_err()
    );
}
