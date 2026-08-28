//! This baker's flattening of a `docker save` archive, against `docker
//! export`'s flattening of the same image.
//!
//! An example rather than a test because it needs a docker daemon, which the
//! rest of the suite does not — the suite's deterministic archives are in
//! `baker/tests/archive.rs`. This is the oracle for the real thing, and it is
//! kept runnable so that "it matched docker" stays a claim anyone can check:
//!
//! ```text
//! docker save <image> -o saved.tar
//! docker create --name c <image> true && docker export c -o flat.tar
//! cargo run -p baker --example image_differential -- saved.tar flat.tar
//! ```
//!
//! `docker export` exports a *container*, so it carries a handful of files
//! the runtime injects and the image does not have — `.dockerenv`,
//! `/dev/{console,pts,shm}`, and the bind-mounted `/etc/{hosts,hostname,
//! resolv.conf}`. Those are reported like any other difference; they are the
//! expected ones.

use std::collections::BTreeMap;

use baker::tree::{Body, ROOT, Tree};

fn walk(tree: &Tree, id: usize, prefix: &str, into: &mut BTreeMap<String, String>) {
    let node = tree.node(id);
    let shape = match &node.body {
        Body::Directory(entries) => {
            for (name, child) in entries {
                let name = String::from_utf8_lossy(name);
                let path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                walk(tree, *child, &path, into);
            }
            format!("dir {:o}", node.meta.mode)
        }
        Body::Regular(bytes) => format!("file {:o} {} bytes", node.meta.mode, bytes.len()),
        Body::Symlink(target) => format!(
            "link {:o} -> {}",
            node.meta.mode,
            String::from_utf8_lossy(target)
        ),
        Body::Special => format!("special {:o} {}", node.meta.mode, node.meta.rdev),
    };
    if !prefix.is_empty() {
        into.insert(prefix.to_string(), shape);
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let saved = arguments
        .next()
        .expect("usage: <docker-save.tar> <docker-export.tar>");
    let exported = arguments
        .next()
        .expect("usage: <docker-save.tar> <docker-export.tar>");

    let tree = baker::layers::tree_from_archive(&std::fs::read(&saved).expect("read the archive"))
        .expect("flatten the layer stack");
    let mut ours = BTreeMap::new();
    walk(&tree, ROOT, "", &mut ours);

    let flat = baker::tar::read(&std::fs::read(&exported).expect("read the export"))
        .expect("read the export");
    let mut theirs: BTreeMap<String, String> = BTreeMap::new();
    for entry in &flat {
        let path = String::from_utf8_lossy(&entry.path);
        let path = path
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        if path.is_empty() {
            continue;
        }
        let shape = match entry.kind {
            baker::tar::Kind::Directory => format!("dir {:o}", 0o040000 | (entry.mode & 0o7777)),
            baker::tar::Kind::Regular => format!(
                "file {:o} {} bytes",
                0o100000 | (entry.mode & 0o7777),
                entry.contents.len()
            ),
            // A hardlink's shape is its twin's; compared by identity below
            // rather than by these strings.
            baker::tar::Kind::HardLink => "hardlink".to_string(),
            baker::tar::Kind::Symlink => format!(
                "link {:o} -> {}",
                0o120000 | (entry.mode & 0o7777),
                String::from_utf8_lossy(&entry.link)
            ),
            baker::tar::Kind::Fifo => format!("special {:o} 0", 0o010000 | (entry.mode & 0o7777)),
            baker::tar::Kind::Character => "special-chr".to_string(),
            baker::tar::Kind::Block => "special-blk".to_string(),
        };
        theirs.insert(path, shape);
    }

    let mut differences = 0;
    for (path, shape) in &theirs {
        if shape == "hardlink" {
            continue;
        }
        match ours.get(path) {
            None => {
                println!("MISSING from our flatten: {path} ({shape})");
                differences += 1;
            }
            Some(mine) if mine != shape => {
                println!("DIFFERS {path}: ours `{mine}`, docker `{shape}`");
                differences += 1;
            }
            Some(_) => {}
        }
    }
    for (path, shape) in &ours {
        if !theirs.contains_key(path) {
            println!("EXTRA in our flatten: {path} ({shape})");
            differences += 1;
        }
    }
    println!(
        "{} paths ours, {} paths docker, {differences} differences",
        ours.len(),
        theirs.len()
    );
}
