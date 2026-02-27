use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use fnn::fiber::channel::ChannelActorStateStore;
use fnn::store::Store;

fn resolve_db_dir(node_dir: &Path) -> Result<PathBuf, String> {
    let mut candidates = fs::read_dir(node_dir)
        .map_err(|e| format!("read_dir {}: {e}", node_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Err(format!(
            "no rocksdb dir found under node dir {}",
            node_dir.display()
        ));
    }
    candidates.sort();
    Ok(candidates[0].clone())
}

fn dump_node(node_dir: &Path) -> Result<(), String> {
    let db_dir = resolve_db_dir(node_dir)?;
    let store = Store::new(&db_dir)?;

    println!(
        "== node_dir={} db_dir={} ==",
        node_dir.display(),
        db_dir.display()
    );
    let mut states = store.get_channel_states(None);
    states.sort_by(|(_, a, _), (_, b, _)| a.as_ref().cmp(b.as_ref()));
    println!("channel_count={}", states.len());

    for (peer_id, channel_id, channel_state) in states {
        if let Some(actor_state) = store.get_channel_actor_state(&channel_id) {
            let tlc_count = actor_state.tlc_state.all_tlcs().count();
            println!(
                "channel={:x} peer={} state={:?} closed={} reestablishing={} waiting_ack={} retry_ops={} tlcs={}",
                channel_id,
                peer_id,
                channel_state,
                actor_state.is_closed(),
                actor_state.reestablishing,
                actor_state.is_waiting_tlc_ack(),
                actor_state.retryable_tlc_operations.len(),
                tlc_count
            );
        } else {
            println!(
                "channel={:x} peer={} state={:?} actor_state=MISSING",
                channel_id, peer_id, channel_state
            );
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        return Err(
            "usage: cargo run -p fnn --example dump_channel_states -- <node_dir> [node_dir ...]"
                .to_string(),
        );
    }
    for arg in args {
        dump_node(Path::new(&arg))?;
    }
    Ok(())
}
