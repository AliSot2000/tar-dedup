use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path};

fn convert_bool(b: bool) -> char {
    match b {
        true => 't',
        false => 'f',
    }
}

fn main(){

    let target_dir = "/home/alisot2000/Documents/06_ReposNCode/tar-dedup/scratch/all-types";
    let dir=  fs::read_dir(target_dir).ok().unwrap();



    for special in dir {
        let de = match special {
            Ok(e) => e,
            Err(e) => {
                println!("Error while accessing: {e}");
                continue;
            },
        };
        let fbn = de.file_name();
        let file_name = fbn.to_string_lossy();
        let metadata = match de.metadata() {
            Ok(r) => r,
            Err(e) => {
                println!("Encountered error {e}");
                continue;
            },
        };
        let ft = metadata.file_type();

        let is_file = convert_bool(ft.is_file());
        let is_dir = convert_bool(ft.is_dir());
        let is_symlink = convert_bool(ft.is_symlink());

        let is_blockdev = convert_bool(ft.is_block_device());
        let is_chardev  = convert_bool(ft.is_char_device());
        let is_socket = convert_bool(ft.is_socket());
        let is_fifo = convert_bool(ft.is_fifo());

        // let is_dir_symlink = convert_bool(ft.is_symlink_dir());
        // let is_file_symlink = convert_bool(ft.is_symlink_file());

        println!("File Name: {file_name:10} File Type casting: file: {is_file}, dir: {is_dir}, is_sym: {is_symlink}, block device: {is_blockdev}, char device: {is_chardev}, fifo: {is_fifo}, socket: {is_socket}, Permissions: {:#0b}", metadata.mode() & 0o7777);
    }
}