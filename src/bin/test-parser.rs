use anyhow::Result;
use loft::nix_utils as nix;
use std::collections::HashSet;
use tracing::{error, info, Level};

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = tracing_subscriber::fmt();
    subscriber.with_max_level(Level::DEBUG).init();
    // 1. ✅ Define a list of keys to skip instead of just one.
    let keys_to_skip = vec![
        "cache.nixos.org-1".to_string(),
        // You can add other keys here, for example:
        // "another-cache.org-1".to_string(),
    ];

    let all_sigs_map = nix::get_all_path_signatures().await?;
    // filter out the signatures to only ones we need.
    let filtered_paths = nix::filter_out_sig_keys(all_sigs_map, keys_to_skip.clone()).await?;

    info!(
        "\nFound {} paths that are NOT signed by any of the specified keys.",
        filtered_paths.len(),
    );

    for (path, sigs) in filtered_paths.iter().take(5) {
        info!("  - Path: {}", path);
        for sig in sigs.iter() {
            if let nix::Signature::Crypto { key_name, .. } = sig {
                info!("    - Sig Key: {}", key_name);
            }
        }
    }

    let filtered_paths_vec: Vec<String> = filtered_paths.keys().cloned().collect();

    let all_closure_paths: HashSet<String> =
        match nix::get_store_paths_closure(filtered_paths_vec).await {
            Ok(paths) => paths.into_iter().collect(),
            Err(e) => {
                error!("Failed to get closures: {:?}", e);
                HashSet::new()
            }
        };

    info!("Total unique closure paths: {}", all_closure_paths.len());

    let closure_signatures =
        nix::get_path_signatures(all_closure_paths.into_iter().collect()).await?;

    let filtered_closure_paths = nix::filter_out_sig_keys(closure_signatures, keys_to_skip).await?;

    info!("Checking closure path sigs:");
    info!("Filtered closure length: {}", filtered_closure_paths.len());
    for (path, sigs) in filtered_closure_paths.iter().take(5) {
        info!("  - Path: {}", path);
        for sig in sigs.iter() {
            if let nix::Signature::Crypto { key_name, .. } = sig {
                info!("    - Sig Key: {}", key_name);
            }
        }
    }

    Ok(())
}
