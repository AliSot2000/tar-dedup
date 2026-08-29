use std::env;
use std::path::Path;
use path_clean::PathClean;
use walkdir::WalkDir;
use regex::Regex;


fn convert_bool(b: bool) -> char {
    match b {
        true => 't',
        false => 'f',
    }
}

fn main(){

    // let target_dir = Path::new("/home/alisot2000/Documents/06_ReposNCode/tar-dedup/scratch");
    // //let target_dir = Path::new("./scratch");
    // println!("Path is absolute: {}", target_dir.is_absolute());
    //
    //
    // let path = env::current_dir().unwrap();
    //
    // println!("CWD: {}", path.to_string_lossy());
    //
    // let dir =  WalkDir::new(target_dir)
    //     .follow_links(false)
    //     .same_file_system(false)
    //     .into_iter();
    //
    //
    //
    //
    // for special in dir {
    //     let de = match special {
    //         Ok(e) => e,
    //         Err(e) => {
    //             println!("Error while accessing: {e}");
    //             continue;
    //         },
    //     };
    //     println!("fp: {}", de.path().to_path_buf().to_string_lossy());
    //
    // }
    // let p = Path::new("/").parent();
    // println!("PathTest: {p}");
    // assert!(Regex::new("^^foo").is_ok());
    let p = Path::new("/hello/world/");
    println!("Cleaned Path: {}", p.clean().to_string_lossy());

}