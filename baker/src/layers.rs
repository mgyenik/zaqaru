//! A `docker save` archive: layers laid over each other into one tree.
//!
//! An image is a stack, and the stack's semantics are entirely about
//! *deletion*. A tar archive can only say "here is a file"; saying "the layer
//! below me had a file here and it is gone" needs a convention, and the
//! convention is a name. `.wh.foo` beside where `foo` would be means `foo` is
//! deleted; `.wh..wh..opq` in a directory means everything the layers below
//! put in that directory is gone, whatever it was called.
//!
//! Those markers are applied *here*, at bake time, and never reach the image.
//! A runtime that had to know about them would be a runtime that had to know
//! about layers, and the whole point of baking is that the container sees a
//! filesystem rather than a stack.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use kisal::image::file_type;

use crate::json;
use crate::tar::{self, Entry, Kind};
use crate::tree::{Body, Meta, NodeId, ROOT, Tree};

/// The prefix that marks a deletion.
const WHITEOUT: &[u8] = b".wh.";
/// The whole name that marks a directory opaque.
const OPAQUE: &[u8] = b".wh..wh..opq";

/// Flattens a `docker save` archive into one tree.
pub fn tree_from_archive(archive: &[u8]) -> Result<Tree> {
    let members = tar::read(archive).context("reading the image archive")?;
    let order = layer_order(&members)?;

    let mut tree = Tree::new();
    for name in &order {
        let layer = members
            .iter()
            .find(|member| normalise(&member.path) == name.as_bytes())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the manifest names the layer `{name}`, which the archive \
                     does not contain"
                )
            })?;
        // Modern `docker save` writes an OCI layout whose layer blobs are
        // gzip-compressed — verified against docker 29.1.3, whose
        // `blobs/sha256/…` layers begin `1f 8b`. Older archives, and layers
        // written by `tar` directly, are not. Both are read, decided by what
        // the bytes say rather than by what the name suggests.
        let layer = if layer.contents.starts_with(&[0x1f, 0x8b]) {
            inflate(&layer.contents)
                .with_context(|| format!("decompressing the layer `{name}`"))?
        } else {
            layer.contents.clone()
        };
        let entries =
            tar::read(&layer).with_context(|| format!("reading the layer `{name}`"))?;
        apply(&mut tree, &entries)
            .with_context(|| format!("applying the layer `{name}`"))?;
    }
    Ok(tree)
}

/// The layer paths, in the order they must be applied.
///
/// Order is the whole meaning of a stack: the same path in two layers is one
/// file, and which one it is depends only on this list.
fn layer_order(members: &[Entry]) -> Result<Vec<String>> {
    let manifest = members
        .iter()
        .find(|member| normalise(&member.path) == b"manifest.json")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the archive has no `manifest.json`, so there is nothing that \
                 says what its layers are or what order they go in. `docker \
                 save` writes one; an OCI layout directory does not, and is \
                 not what this reads."
            )
        })?;
    let document = json::parse(&manifest.contents).context("parsing `manifest.json`")?;
    let images = document
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("`manifest.json` is not an array of images"))?;
    let [image] = images else {
        bail!(
            "`manifest.json` describes {} images. An archive of several is a \
             choice about which one to bake, and nothing here has made it.",
            images.len()
        );
    };
    let layers = image
        .get("Layers")
        .ok_or_else(|| anyhow::anyhow!("the manifest entry has no `Layers`"))?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("the manifest's `Layers` is not an array"))?;
    layers
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("a layer in `Layers` is not a string"))
        })
        .collect()
}

/// Lays one layer's entries over what is already there.
fn apply(tree: &mut Tree, entries: &[Entry]) -> Result<()> {
    for entry in entries {
        let path = normalise(&entry.path);
        if path.is_empty() {
            // The archive's own root entry: metadata for `/`, which a base
            // image does carry.
            tree.node_mut(ROOT).meta = meta_of(entry);
            continue;
        }
        let Some((directory, name)) = tree.place(&path)? else {
            tree.node_mut(ROOT).meta = meta_of(entry);
            continue;
        };

        if name == OPAQUE {
            // Everything the layers below put in this directory is gone.
            // Not the directory itself: this layer's own entries for it
            // have already been applied or are still to come.
            if let Body::Directory(children) = &mut tree.node_mut(directory).body {
                *children = BTreeMap::new();
            }
            continue;
        }
        if let Some(deleted) = name.strip_prefix(WHITEOUT) {
            if deleted.is_empty() {
                bail!("a layer carries `.wh.` with nothing after it");
            }
            tree.unlink(directory, deleted);
            continue;
        }

        let node = match entry.kind {
            // A hardlink names an entry the archive has already placed —
            // in *this* layer or in one below it, since the stack is one
            // filesystem by the time the link is applied.
            Kind::HardLink => {
                let target = normalise(&entry.link);
                tree.resolve(&target).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{}` is a hard link to `{}`, which is not in the image",
                        String::from_utf8_lossy(&path),
                        String::from_utf8_lossy(&target)
                    )
                })?
            }
            Kind::Directory => {
                // A directory that is already there keeps its children and
                // takes this layer's metadata: two layers both carrying
                // `/usr` is the normal case, not a conflict.
                match tree.lookup(directory, &name) {
                    Some(existing) if tree.node(existing).is_directory() => {
                        tree.node_mut(existing).meta = meta_of(entry);
                        existing
                    }
                    _ => tree.add(meta_of(entry), Body::Directory(BTreeMap::new())),
                }
            }
            Kind::Regular => tree.add(meta_of(entry), Body::Regular(entry.contents.clone())),
            Kind::Symlink => tree.add(meta_of(entry), Body::Symlink(entry.link.clone())),
            Kind::Character | Kind::Block | Kind::Fifo => {
                tree.add(meta_of(entry), Body::Special)
            }
        };
        tree.link(directory, &name, node)
            .with_context(|| format!("placing `{}`", String::from_utf8_lossy(&path)))?;
    }
    Ok(())
}

/// A tar header's metadata, as the tree holds it.
fn meta_of(entry: &Entry) -> Meta {
    let kind = match entry.kind {
        Kind::Directory => file_type::DIRECTORY,
        Kind::Symlink => file_type::SYMLINK,
        Kind::Character => file_type::CHARACTER,
        Kind::Block => file_type::BLOCK,
        Kind::Fifo => file_type::FIFO,
        Kind::Regular | Kind::HardLink => file_type::REGULAR,
    };
    Meta {
        // Tar's mode field is permission bits only — the type is the
        // typeflag, and a header that also set type bits would be a header
        // no writer produces. They are combined here, once.
        mode: kind | (entry.mode & 0o7777),
        uid: entry.uid,
        gid: entry.gid,
        mtime_sec: entry.mtime_sec,
        mtime_nsec: entry.mtime_nsec,
        rdev: match entry.kind {
            Kind::Character | Kind::Block => {
                // Linux's packed `dev_t`, which is not a plain shift: major
                // occupies bits 8..20, and minor bits 0..8 and 20..32.
                ((entry.device_major as u64 & 0xfff) << 8)
                    | (entry.device_minor as u64 & 0xff)
                    | ((entry.device_minor as u64 & 0xfff00) << 12)
            }
            _ => 0,
        },
        uname: entry.uname.clone(),
        gname: entry.gname.clone(),
        xattrs: entry.xattrs.clone(),
    }
}

/// Decompresses a gzip member.
///
/// The one thing in this crate that is not hand-written. The tar and JSON
/// readers are, because their failure mode is accepting a document that
/// silently means something else; DEFLATE either produces the bytes or
/// fails, so a decoder written here would buy nothing and risk a class of
/// bug nothing in this project could detect.
fn inflate(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(out)
}

/// A tar path as the tree names it: no leading `./` or `/`, no trailing `/`.
fn normalise(path: &[u8]) -> Vec<u8> {
    let mut path = path;
    while let Some(rest) = path.strip_prefix(b"./") {
        path = rest;
    }
    while let Some(rest) = path.strip_prefix(b"/") {
        path = rest;
    }
    while let Some(rest) = path.strip_suffix(b"/") {
        path = rest;
    }
    if path == b"." {
        return Vec::new();
    }
    path.to_vec()
}

/// The node a path names, for a caller that already has a tree.
pub fn resolve(tree: &Tree, path: &str) -> Option<NodeId> {
    tree.resolve(&normalise(path.as_bytes()))
}
