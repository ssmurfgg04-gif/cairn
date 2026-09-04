//! Serve a review portal for ONE project root (development/smoke):
//!
//! ```sh
//! cargo run -p cairn-review --example serve -- /path/to/project 127.0.0.1:17778
//! ```
//!
//! Then open the printed link. The real daemon serves the same router
//! over all attached runtimes (`cairn daemon --review`); this example is
//! the minimal one-root harness.

use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: serve <root> [addr]"));
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:17778".to_string());

    struct OneRoot(PathBuf);
    #[async_trait::async_trait]
    impl cairn_review::http::RootProvider for OneRoot {
        async fn roots(&self) -> Vec<(String, PathBuf)> {
            vec![("project".into(), self.0.clone())]
        }
    }

    let portal = cairn_review::http::Portal::new(Arc::new(OneRoot(root)));
    println!("portal on http://{addr}/ — open /r/<token> with a link from `cairn review link`");
    cairn_review::http::serve(addr, portal).await.expect("portal serve");
}
