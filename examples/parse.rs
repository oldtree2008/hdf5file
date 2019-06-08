#[macro_use]
extern crate trackable;

use hdf5file;
use std::fs::File;
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
struct Opt {
    path: PathBuf,
}

fn main() -> trackable::result::TopLevelResult {
    let opt = Opt::from_args();
    let mut file = track_any_err!(File::open(&opt.path); opt.path)?;

    let s = track!(hdf5file::level0::Superblock::from_reader(&mut file))?;
    println!("Superblock: {:?}", s);
    println!(
        "Root Link Name: {:?}",
        s.root_group_symbol_table_entry.link_name(&mut file)?
    );

    let h = track!(s.root_group_symbol_table_entry.object_header(&mut file))?;
    println!("Root Object Header: {:?}", h);

    let h = track!(s.root_group_symbol_table_entry.local_heaps(&mut file))?;
    println!("Local Heaps: {:?}", h);

    let b = track!(s.root_group_symbol_table_entry.b_tree_node(&mut file))?;
    println!("B-Tree Node: {:?}", b);

    for k in track!(b.keys(h, &mut file))? {
        println!("  - Key: {:?}", track!(k)?);
    }

    let mut stack = vec![b];
    while let Some(node) = stack.pop() {
        for c in track!(node.children(&mut file))? {
            stack.push(track!(c)?);
        }
        println!("STACK: {}", stack.len());
    }
    Ok(())
}
