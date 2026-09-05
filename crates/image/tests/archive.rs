//! The layer stack: whiteouts, opaque directories, order, and what an
//! archive is refused for.
//!
//! The archives are built here rather than by a docker daemon, so what runs
//! in the suite is deterministic and needs nothing but `tar`. The daemon's
//! own answer is the oracle for the real thing and it is a separate,
//! runnable check — `cargo run -p packager --example image_differential` — kept
//! out of the suite because a test that needs a daemon is a test that stops
//! being run.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use image::tree::{Body, Tree};

struct Workspace {
    root: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn workspace(label: &str) -> Workspace {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("packager-archive-{label}-{unique}"));
    std::fs::create_dir_all(&root).expect("mkdir");
    Workspace { root }
}

impl Workspace {
    /// Archives a directory into a layer blob, as an image layer is.
    fn layer(&self, tree: &Path, name: &str, arguments: &[&str]) -> PathBuf {
        let output = self.root.join("outer/blobs/sha256").join(name);
        std::fs::create_dir_all(output.parent().expect("parent")).expect("mkdir");
        let status = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&output)
            .arg("-C")
            .arg(tree)
            .args(arguments)
            .arg(".")
            .status()
            .expect("run tar");
        assert!(status.success(), "tar failed");
        output
    }

    /// Assembles the outer archive a `docker save` produces: a manifest
    /// naming its layers, in order, and the layer blobs beside it.
    fn archive(&self, layers: &[&str]) -> Vec<u8> {
        let names: Vec<String> = layers
            .iter()
            .map(|name| format!("\"blobs/sha256/{name}\""))
            .collect();
        let manifest = format!(
            "[{{\"Config\":\"blobs/sha256/config\",\"RepoTags\":[\"test:latest\"],\
             \"Layers\":[{}]}}]",
            names.join(",")
        );
        std::fs::write(self.root.join("outer/manifest.json"), manifest).expect("write");
        let output = self.root.join("image.tar");
        let status = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&output)
            .arg("-C")
            .arg(self.root.join("outer"))
            .arg(".")
            .status()
            .expect("run tar");
        assert!(status.success(), "tar failed");
        std::fs::read(&output).expect("read")
    }

    fn tree_directory(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut file = std::fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

fn contents(tree: &Tree, path: &str) -> Option<Vec<u8>> {
    let id = image::layers::resolve(tree, path)?;
    match &tree.node(id).body {
        Body::Regular(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

/// The whole of what a layer stack means: a later layer replaces, deletes,
/// and adds, and the result is one filesystem with no trace of any of it.
#[test]
fn a_later_layer_replaces_deletes_and_adds() {
    let workspace = workspace("stack");

    let base = workspace.tree_directory("base");
    write(&base.join("etc/hosts"), b"base hosts\n");
    write(&base.join("etc/release"), b"base\n");
    write(&base.join("data/keep"), b"kept\n");
    write(&base.join("data/doomed"), b"doomed\n");
    std::os::unix::fs::symlink("data", base.join("link")).expect("symlink");
    workspace.layer(&base, "layer0", &["--format=pax"]);

    let over = workspace.tree_directory("over");
    // Replace one file, delete another, add a third.
    write(&over.join("etc/hosts"), b"replaced hosts\n");
    write(&over.join("data/.wh.doomed"), b"");
    write(&over.join("etc/.wh.release"), b"");
    write(&over.join("data/added"), b"added\n");
    workspace.layer(&over, "layer1", &["--format=pax"]);

    let archive = workspace.archive(&["layer0", "layer1"]);
    let tree = image::layers::tree_from_archive(&archive).expect("flatten");

    assert_eq!(
        contents(&tree, "etc/hosts").as_deref(),
        Some(&b"replaced hosts\n"[..])
    );
    assert_eq!(
        contents(&tree, "data/keep").as_deref(),
        Some(&b"kept\n"[..])
    );
    assert_eq!(
        contents(&tree, "data/added").as_deref(),
        Some(&b"added\n"[..])
    );
    assert!(
        image::layers::resolve(&tree, "data/doomed").is_none(),
        "the whiteout did not delete the file"
    );
    assert!(image::layers::resolve(&tree, "etc/release").is_none());
    // And the markers themselves never reach the image: a runtime that had
    // to know about them would be a runtime that knew about layers.
    assert!(image::layers::resolve(&tree, "data/.wh.doomed").is_none());
    assert!(image::layers::resolve(&tree, "etc/.wh.release").is_none());
    // The directories that held them are still there.
    assert!(image::layers::resolve(&tree, "data").is_some());
    assert!(image::layers::resolve(&tree, "link").is_some());
}

/// An opaque marker deletes a directory's whole contents, whatever they were
/// called — which is what a layer that removed and recreated a directory
/// produces, and the only way to say "everything below is gone" in an
/// archive that can only list files.
#[test]
fn an_opaque_directory_hides_everything_beneath_it() {
    let workspace = workspace("opaque");

    let base = workspace.tree_directory("base");
    write(&base.join("site/one"), b"one\n");
    write(&base.join("site/two"), b"two\n");
    write(&base.join("site/deep/three"), b"three\n");
    write(&base.join("other/kept"), b"kept\n");
    workspace.layer(&base, "layer0", &["--format=pax"]);

    let over = workspace.tree_directory("over");
    write(&over.join("site/.wh..wh..opq"), b"");
    write(&over.join("site/fresh"), b"fresh\n");
    workspace.layer(&over, "layer1", &["--format=pax"]);

    let tree = image::layers::tree_from_archive(&workspace.archive(&["layer0", "layer1"]))
        .expect("flatten");

    assert_eq!(
        contents(&tree, "site/fresh").as_deref(),
        Some(&b"fresh\n"[..])
    );
    for gone in ["site/one", "site/two", "site/deep", "site/deep/three"] {
        assert!(
            image::layers::resolve(&tree, gone).is_none(),
            "`{gone}` survived an opaque marker"
        );
    }
    // Only that directory: an opaque marker is not a whiteout of the world.
    assert_eq!(
        contents(&tree, "other/kept").as_deref(),
        Some(&b"kept\n"[..])
    );
    assert!(image::layers::resolve(&tree, "site/.wh..wh..opq").is_none());
}

/// Order is the whole meaning of a stack, and it comes from the manifest
/// rather than from the archive's own layout.
#[test]
fn the_manifest_decides_which_layer_wins() {
    let workspace = workspace("order");
    let first = workspace.tree_directory("first");
    write(&first.join("who"), b"first\n");
    workspace.layer(&first, "layer0", &["--format=pax"]);
    let second = workspace.tree_directory("second");
    write(&second.join("who"), b"second\n");
    workspace.layer(&second, "layer1", &["--format=pax"]);

    let forward = image::layers::tree_from_archive(&workspace.archive(&["layer0", "layer1"]))
        .expect("flatten");
    assert_eq!(contents(&forward, "who").as_deref(), Some(&b"second\n"[..]));

    let reversed = image::layers::tree_from_archive(&workspace.archive(&["layer1", "layer0"]))
        .expect("flatten");
    assert_eq!(
        contents(&reversed, "who").as_deref(),
        Some(&b"first\n"[..]),
        "the manifest's order decides, not the archive's"
    );
}

/// A hardlink record names a path rather than carrying bytes, and the two
/// names have to end up as one file — `nlink` and every hardlink detector
/// depend on it, and a base image is full of them.
#[test]
fn a_hardlink_record_names_one_file_twice() {
    let workspace = workspace("hardlink");
    let base = workspace.tree_directory("base");
    write(&base.join("bin/tool"), b"the tool\n");
    std::fs::hard_link(base.join("bin/tool"), base.join("bin/tool2")).expect("link");
    workspace.layer(&base, "layer0", &["--format=pax"]);

    let tree = image::layers::tree_from_archive(&workspace.archive(&["layer0"])).expect("flatten");
    let one = image::layers::resolve(&tree, "bin/tool").expect("bin/tool");
    let two = image::layers::resolve(&tree, "bin/tool2").expect("bin/tool2");
    assert_eq!(one, two, "two names, one node");

    // And the bake turns that into one inode with `nlink` 2.
    let image = image::bake_tree(&tree).expect("bake");
    let image = kernel::image::Image::parse(&image.index, &image.blob).expect("parse");
    let bin = image.root();
    let bin = image.inode(bin).expect("root");
    let bin = image.lookup(&bin, b"bin").expect("lookup").expect("bin");
    let bin = image.inode(bin.inode).expect("inode");
    let tool = image.lookup(&bin, b"tool").expect("lookup").expect("tool");
    let twin = image
        .lookup(&bin, b"tool2")
        .expect("lookup")
        .expect("tool2");
    assert_eq!(tool.inode, twin.inode);
    assert_eq!(image.inode(tool.inode).expect("inode").nlink, 2);
}

/// Metadata a tar header carries and a directory on disk does not: symbolic
/// owner names, and the extended attributes that hold file capabilities.
#[test]
fn tar_metadata_reaches_the_baked_index() {
    let workspace = workspace("metadata");
    let base = workspace.tree_directory("base");
    write(&base.join("bin/ping"), b"binary\n");
    set_xattr(
        &base.join("bin/ping"),
        b"user.capability",
        &[0x01, 0x00, 0x00, 0x02],
    );
    workspace.layer(
        &base,
        "layer0",
        &[
            "--format=pax",
            "--xattrs",
            "--owner=root:0",
            "--group=wheel:0",
        ],
    );

    let tree = image::layers::tree_from_archive(&workspace.archive(&["layer0"])).expect("flatten");
    let ping = image::layers::resolve(&tree, "bin/ping").expect("bin/ping");
    let meta = &tree.node(ping).meta;
    assert_eq!(meta.uname, b"root", "tar's symbolic owner name");
    assert_eq!(meta.gname, b"wheel");
    assert_eq!(
        meta.xattrs,
        vec![(b"user.capability".to_vec(), vec![0x01, 0x00, 0x00, 0x02])]
    );

    // Through the bake, and back out of the index the kernel reads.
    let baked = image::bake_tree(&tree).expect("bake");
    let image = kernel::image::Image::parse(&baked.index, &baked.blob).expect("parse");
    let root = image.inode(image.root()).expect("root");
    let bin = image.lookup(&root, b"bin").expect("lookup").expect("bin");
    let bin = image.inode(bin.inode).expect("inode");
    let ping = image.lookup(&bin, b"ping").expect("lookup").expect("ping");
    let ping = image.inode(ping.inode).expect("inode");
    assert_eq!(image.string(ping.uname_ref).expect("uname"), b"root");
    assert_eq!(image.string(ping.gname_ref).expect("gname"), b"wheel");
    assert_eq!(image.xattr_count(&ping).expect("count"), 1);
    let (name, value) = image.xattr(&ping, 0).expect("xattr");
    assert_eq!(name, b"user.capability");
    assert_eq!(value, &[0x01, 0x00, 0x00, 0x02]);
}

/// A gzip-compressed layer, which is what `docker save` writes: verified
/// against docker 29.1.3, whose `blobs/sha256/…` layers begin `1f 8b`. What
/// decides is the bytes, not the name — a blob named by its digest says
/// nothing about its encoding.
#[test]
fn a_compressed_layer_is_decompressed() {
    let workspace = workspace("gzip");
    let base = workspace.tree_directory("base");
    write(&base.join("etc/hosts"), b"compressed\n");
    let path = workspace.layer(&base, "layer0", &["--format=pax"]);

    let plain = std::fs::read(&path).expect("read");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&plain).expect("compress");
    std::fs::write(&path, encoder.finish().expect("finish")).expect("write");

    let tree = image::layers::tree_from_archive(&workspace.archive(&["layer0"])).expect("flatten");
    assert_eq!(
        contents(&tree, "etc/hosts").as_deref(),
        Some(&b"compressed\n"[..])
    );
}

/// An archive bakes end to end, and the image reads back through the parser
/// the kernel uses.
#[test]
fn an_archive_bakes_into_an_image() {
    let workspace = workspace("bake");
    let base = workspace.tree_directory("base");
    write(&base.join("etc/hosts"), b"127.0.0.1 localhost\n");
    write(&base.join("usr/bin/tool"), b"#!/bin/sh\n");
    std::fs::set_permissions(
        base.join("usr/bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::os::unix::fs::symlink("usr/bin", base.join("bin")).expect("symlink");
    workspace.layer(&base, "layer0", &["--format=pax"]);

    let baked = image::bake_archive(&workspace.archive(&["layer0"])).expect("bake");
    let image = kernel::image::Image::parse(&baked.index, &baked.blob).expect("parse");
    let root = image.inode(image.root()).expect("root");
    let etc = image.lookup(&root, b"etc").expect("lookup").expect("etc");
    let etc = image.inode(etc.inode).expect("inode");
    let hosts = image
        .lookup(&etc, b"hosts")
        .expect("lookup")
        .expect("hosts");
    let hosts = image.inode(hosts.inode).expect("inode");
    assert_eq!(
        image.contents(&hosts).expect("contents"),
        b"127.0.0.1 localhost\n"
    );
    assert_eq!(hosts.mode & 0o170000, 0o100000);

    let link = image.lookup(&root, b"bin").expect("lookup").expect("bin");
    let link = image.inode(link.inode).expect("inode");
    assert_eq!(image.symlink_target(&link).expect("target"), b"usr/bin");
}

/// What an archive is refused for. Each of these would otherwise produce an
/// image that is quietly not the one that was asked for.
#[test]
fn a_malformed_archive_is_refused_by_name() {
    let workspace = workspace("refusals");
    let base = workspace.tree_directory("base");
    write(&base.join("file"), b"x\n");
    workspace.layer(&base, "layer0", &["--format=pax"]);

    // A manifest naming a layer the archive does not carry.
    let refusal = image::layers::tree_from_archive(&workspace.archive(&["layer0", "missing"]))
        .expect_err("a missing layer must be refused");
    assert!(format!("{refusal:#}").contains("missing"), "{refusal:#}");

    // No manifest at all: the layers are there and nothing says what order
    // they go in, which is not an image.
    std::fs::remove_file(workspace.root.join("outer/manifest.json")).expect("remove");
    let status = std::process::Command::new("tar")
        .arg("-cf")
        .arg(workspace.root.join("image.tar"))
        .arg("-C")
        .arg(workspace.root.join("outer"))
        .arg(".")
        .status()
        .expect("tar");
    assert!(status.success());
    let without = std::fs::read(workspace.root.join("image.tar")).expect("read");
    let refusal =
        image::layers::tree_from_archive(&without).expect_err("no manifest must be refused");
    assert!(
        format!("{refusal:#}").contains("manifest.json"),
        "{refusal:#}"
    );
}

/// A layer entry naming a path outside the image is refused rather than
/// followed. An archive is untrusted input: `docker save` does not produce
/// one of these, and something else might.
#[test]
fn a_path_that_climbs_out_of_the_image_is_refused() {
    let mut tree = Tree::new();
    let refusal = tree
        .place(b"../escape")
        .expect_err("a climbing path must be refused");
    assert!(format!("{refusal}").contains("climbs out"), "{refusal}");
    let refusal = tree
        .place(b"usr/../../escape")
        .expect_err("a climbing path must be refused");
    assert!(format!("{refusal}").contains("climbs out"), "{refusal}");
}

fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    let name = std::ffi::CString::new(name).expect("name");
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

/// The two front ends agree.
///
/// A rootfs directory and an archive of that same directory are the two ways
/// an image arrives, and they share everything below the tree. This is what
/// says so: the same tree, baked both ways, resolves to the same files with
/// the same metadata. A divergence here would be the two front ends
/// disagreeing about what a tree *is*, which is the class of bug the shared
/// tree exists to remove.
#[test]
fn a_directory_and_an_archive_of_it_bake_alike() {
    let workspace = workspace("front-ends");
    let source = workspace.tree_directory("source");
    write(&source.join("etc/hosts"), b"127.0.0.1 localhost\n");
    write(&source.join("etc/conf.d/rc"), b"rc\n");
    write(&source.join("usr/bin/tool"), b"#!/bin/sh\n");
    std::fs::set_permissions(
        source.join("usr/bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::hard_link(source.join("usr/bin/tool"), source.join("usr/bin/tool2")).expect("link");
    std::os::unix::fs::symlink("usr/bin", source.join("bin")).expect("symlink");
    std::os::unix::fs::symlink("../hosts", source.join("etc/conf.d/hosts")).expect("symlink");
    set_xattr(&source.join("etc/hosts"), b"user.origin", b"the fixture");

    workspace.layer(&source, "layer0", &["--format=pax", "--xattrs"]);
    let from_archive =
        image::layers::tree_from_archive(&workspace.archive(&["layer0"])).expect("flatten");
    let from_directory = Tree::from_directory(&source).expect("read the directory");

    let mut ours = std::collections::BTreeMap::new();
    let mut theirs = std::collections::BTreeMap::new();
    describe(&from_archive, image::tree::ROOT, "", &mut ours);
    describe(&from_directory, image::tree::ROOT, "", &mut theirs);
    assert_eq!(ours, theirs, "the two front ends built different trees");

    // Including the hardlink's identity, which a path-by-path comparison
    // cannot see: two names, one node, on both sides.
    for tree in [&from_archive, &from_directory] {
        assert_eq!(
            image::layers::resolve(tree, "usr/bin/tool"),
            image::layers::resolve(tree, "usr/bin/tool2")
        );
    }
}

/// Every path in a tree, with what is there — everything the index records
/// except the timestamps, which tar rounds and a filesystem does not.
fn describe(
    tree: &Tree,
    id: usize,
    prefix: &str,
    into: &mut std::collections::BTreeMap<String, String>,
) {
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
                describe(tree, *child, &path, into);
            }
            "dir".to_string()
        }
        Body::Regular(bytes) => format!("file {bytes:?}"),
        Body::Symlink(target) => format!("link {}", String::from_utf8_lossy(target)),
        Body::Special => format!("special {}", node.meta.rdev),
    };
    if prefix.is_empty() {
        return;
    }
    into.insert(
        prefix.to_string(),
        format!(
            "{shape} mode {:o} uid {} gid {} xattrs {:?}",
            node.meta.mode, node.meta.uid, node.meta.gid, node.meta.xattrs
        ),
    );
}

// ---- the image config ------------------------------------------------------

/// Builds a `docker save`-shaped archive by hand: a manifest naming a config
/// blob and one layer, the config, and the layer's tar.
///
/// By hand because the alternative is a test that needs a container runtime
/// and a registry, which is a test nobody runs. What is being checked here
/// is the *reading*, and the reading does not care who wrote the bytes.
fn saved_image(config: &str, layer: &[(&str, &[u8])]) -> Vec<u8> {
    fn member(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let name = path.as_bytes();
        header[..name.len()].copy_from_slice(name);
        header[100..107].copy_from_slice(b"0000644");
        header[108..115].copy_from_slice(b"0000000");
        header[116..123].copy_from_slice(b"0000000");
        let size = format!("{:011o}", contents.len());
        header[124..135].copy_from_slice(size.as_bytes());
        header[136..147].copy_from_slice(b"00000000000");
        header[156] = b'0';
        header[257..262].copy_from_slice(b"ustar");
        header[263..265].copy_from_slice(b"00");
        // The checksum is computed with its own field read as spaces.
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let rendered = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(rendered.as_bytes());

        let mut bytes = header.to_vec();
        bytes.extend_from_slice(contents);
        bytes.resize(bytes.len().next_multiple_of(512), 0);
        bytes
    }

    let mut inner = Vec::new();
    for (path, contents) in layer {
        inner.extend(member(path, contents));
    }
    inner.extend([0u8; 1024]);

    let manifest = r#"[{"Config":"config.json","Layers":["layer.tar"]}]"#;
    let mut archive = Vec::new();
    archive.extend(member("manifest.json", manifest.as_bytes()));
    archive.extend(member("config.json", config.as_bytes()));
    archive.extend(member("layer.tar", &inner));
    archive.extend([0u8; 1024]);
    archive
}

/// An image says what to run and in what environment, and both are read.
///
/// `Entrypoint` and `Cmd` compose in that order — the entrypoint is the
/// program and the command its default arguments — which is what the runtime
/// spec says and what `docker run` does.
#[test]
fn an_image_config_says_what_to_run() {
    let archive = saved_image(
        r#"{"config":{
            "Entrypoint":["/usr/bin/python3"],
            "Cmd":["-c","print(1)"],
            "Env":["PATH=/usr/bin:/bin","LANG=C.UTF-8"],
            "WorkingDir":"/app"
        }}"#,
        &[("./init", b"placeholder")],
    );
    let invocation = image::layers::invocation_from_archive(&archive).expect("read the config");
    assert_eq!(
        invocation.argv,
        vec![
            b"/usr/bin/python3".to_vec(),
            b"-c".to_vec(),
            b"print(1)".to_vec()
        ]
    );
    assert_eq!(
        invocation.environment,
        vec![b"PATH=/usr/bin:/bin".to_vec(), b"LANG=C.UTF-8".to_vec()]
    );
    assert_eq!(invocation.working_directory, b"/app".to_vec());
}

/// An image with only a `Cmd` is the common shape, and an image with neither
/// genuinely does not say what to run — which is reported as nothing rather
/// than guessed at.
#[test]
fn a_config_without_an_entrypoint_is_still_read() {
    let only_command = saved_image(
        r#"{"config":{"Cmd":["python3"]}}"#,
        &[("./init", b"placeholder")],
    );
    let invocation = image::layers::invocation_from_archive(&only_command).expect("read");
    assert_eq!(invocation.argv, vec![b"python3".to_vec()]);
    assert!(invocation.environment.is_empty());

    let silent = saved_image(r#"{"config":{}}"#, &[("./init", b"placeholder")]);
    let invocation = image::layers::invocation_from_archive(&silent).expect("read");
    assert!(invocation.argv.is_empty(), "nothing to run, and it says so");
}

/// The command a caller gives overrides the image's, which is what
/// `docker run image cmd...` means — and the layers are read either way.
#[test]
fn a_given_command_overrides_the_images() {
    let archive = saved_image(
        r#"{"config":{"Cmd":["python3"],"Env":["LANG=C.UTF-8"]}}"#,
        &[("./init", b"placeholder")],
    );
    let (image, invocation) =
        image::bake_archive_as_configured(&archive, &[b"/bin/sh".to_vec()]).expect("bake");
    assert_eq!(invocation.argv, vec![b"/bin/sh".to_vec()]);
    assert_eq!(
        invocation.environment,
        vec![b"LANG=C.UTF-8".to_vec()],
        "the environment is the image's whatever the command is"
    );
    assert!(!image.blob.is_empty(), "the layers were read too");
}
