//! The image as a linked object: the properties that only exist after
//! `wasm-ld` has placed it.
//!
//! Everything else about the packager is checked against the index it produces.
//! What that cannot see is what the *linker* does with the object — whether
//! the blob keeps the page alignment the bake computed its offsets against,
//! whether the symbols resolve at all, and whether the bytes in the running
//! module are the bytes that were baked. Those are the properties the kernel
//! depends on and the ones a round trip through `Image::parse` cannot show.

mod support;

use support::{WorkingDirectory, compile_foreign_wasm_object, try_link_wasm};

/// A C object that reports where the linker put the image.
const PROBE: &str = "\
extern char __image_blob[];\n\
extern char __image_index[];\n\
unsigned blob_address(void) { return (unsigned)(unsigned long)__image_blob; }\n\
unsigned index_address(void) { return (unsigned)(unsigned long)__image_index; }\n";

#[test]
fn the_linker_keeps_the_blob_page_aligned() {
    let workspace = WorkingDirectory::new("image-object");
    let root = workspace.path().join("tree");
    std::fs::create_dir_all(root.join("usr")).expect("mkdir");
    // Big enough that its placement is not accidental, and distinctive
    // enough to find in memory.
    let contents: Vec<u8> = (0..9000u32).map(|index| (index % 251) as u8).collect();
    std::fs::write(root.join("usr/payload"), &contents).expect("write");

    let baked = image::bake_directory(&root).expect("bake");
    let object = bake::object::emit(&baked).expect("emit");
    let object = workspace.write("image.wasm.o", &object);
    let probe = compile_foreign_wasm_object(&workspace, "probe", PROBE);
    let linked = workspace.path().join("image.wasm");
    let outcome = try_link_wasm(
        &[object, probe],
        &linked,
        &[
            "--fatal-warnings",
            "--export=blob_address",
            "--export=index_address",
        ],
    );
    assert!(
        outcome.succeeded,
        "the image object did not link:\n{}",
        outcome.report()
    );

    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::from_file(&engine, &linked).expect("module");
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instantiate");
    let blob_address = instance
        .get_typed_func::<(), u32>(&mut store, "blob_address")
        .expect("blob_address")
        .call(&mut store, ())
        .expect("call");
    let index_address = instance
        .get_typed_func::<(), u32>(&mut store, "index_address")
        .expect("index_address")
        .call(&mut store, ())
        .expect("call");

    // The bake computes a file's `MMAP_ALIGNED` offsets relative to the start
    // of the blob, so alignment inside the segment is alignment relative to
    // nothing unless the segment itself lands on a page.
    assert_eq!(
        blob_address % 4096,
        0,
        "the blob landed at {blob_address}, which is not page-aligned"
    );
    assert_eq!(index_address % 8, 0, "the index has eight-byte fields");

    // And the bytes in the running module are the bytes that were baked.
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("the module has a memory");
    let mut blob = vec![0u8; baked.blob.len()];
    memory
        .read(&store, blob_address as usize, &mut blob)
        .expect("read the blob");
    assert_eq!(blob, baked.blob);
    let mut index = vec![0u8; baked.index.len()];
    memory
        .read(&store, index_address as usize, &mut index)
        .expect("read the index");
    assert_eq!(index, baked.index);

    // The image reads back through the same parser the kernel uses, from the
    // bytes the linker placed rather than from the ones in this process.
    let image = kernel::image::Image::parse(&index, &blob).expect("parse");
    let root_inode = image.inode(image.root()).expect("root");
    let usr = image
        .lookup(&root_inode, b"usr")
        .expect("lookup")
        .expect("/usr exists");
    let usr = image.inode(usr.inode).expect("inode");
    let payload = image
        .lookup(&usr, b"payload")
        .expect("lookup")
        .expect("/usr/payload exists");
    let payload = image.inode(payload.inode).expect("inode");
    assert_eq!(image.contents(&payload).expect("contents"), contents);
}

/// The guard where the narrowing happens.
///
/// A region larger than four gigabytes cannot be recorded in the 32-bit size
/// a wasm data symbol carries, and the packager refuses to build one — but this
/// is the cast, so this is where the check belongs. Testing the arithmetic
/// directly rather than by allocating four gigabytes.
#[test]
fn a_region_too_large_to_address_is_refused() {
    assert!(bake::object::refuse_unaddressable("__image_blob", 0).is_ok());
    assert!(bake::object::refuse_unaddressable("__image_blob", u32::MAX as usize).is_ok());
    let refusal = bake::object::refuse_unaddressable("__image_blob", u32::MAX as usize + 1)
        .expect_err("four gigabytes and one byte does not fit a 32-bit size");
    let text = format!("{refusal}");
    assert!(
        text.contains("__image_blob") && text.contains("4294967296"),
        "{text}"
    );
}
