use std::env;
use std::path::{Component, Path, PathBuf};
use nix::NixPath;
use path_clean::PathClean;
use walkdir::WalkDir;
use regex::Regex;
use tar_dedup::error::Result;


fn convert_bool(b: bool) -> char {
    match b {
        true => 't',
        false => 'f',
    }
}

fn strip_leading_up_1(path: &Path) -> (PathBuf, u64) {


    let parts: Vec<_> = path.components().collect();
    let ups = parts
        .iter()
        .take_while(|c| matches!(c, Component::ParentDir))
        .count();
    let mut out = PathBuf::new();
    for comp in parts.into_iter().skip(ups) {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    (out, ups as u64)
}

fn strip_leading_up_2(path: &Path) -> (PathBuf, u64) {
    let mut components = path.components().peekable();
    let mut ups = 0u64;
    while matches!(components.peek(), Some(Component::ParentDir)) {
        components.next();
        ups += 1;
    }
    let mut out = PathBuf::new();
    for comp in components {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    (out, ups)
}

fn test_strip() -> () {
    let p1 = Path::new("a/b/c/d");
    let p2 = Path::new("../../../");
    let p3 = Path::new("../");
    let p4 = Path::new("../../../u/v/w");
    let p5 = Path::new("../../x/y/z");
    let p6 = Path::new(".");

    let (sp1v, c1v) = strip_leading_up_1(&p1.clean());
    let (sp2v, c2v) = strip_leading_up_1(&p2.clean());
    let (sp3v, c3v) = strip_leading_up_1(&p3.clean());
    let (sp4v, c4v) = strip_leading_up_1(&p4.clean());
    let (sp5v, c5v) = strip_leading_up_1(&p5.clean());
    let (sp6v, c6v) = strip_leading_up_1(&p6.clean());

    println!("Converted {} to {} counted {c1v}", p1.display(), sp1v.display());
    println!("Converted {} to {} counted {c2v}", p2.display(), sp2v.display());
    println!("Converted {} to {} counted {c3v}", p3.display(), sp3v.display());
    println!("Converted {} to {} counted {c4v}", p4.display(), sp4v.display());
    println!("Converted {} to {} counted {c5v}", p5.display(), sp5v.display());
    println!("Converted {} to {} counted {c6v}", p6.display(), sp6v.display());

    let (sp1w, c1w) = strip_leading_up_2(&p1.clean());
    let (sp2w, c2w) = strip_leading_up_2(&p2.clean());
    let (sp3w, c3w) = strip_leading_up_2(&p3.clean());
    let (sp4w, c4w) = strip_leading_up_2(&p4.clean());
    let (sp5w, c5w) = strip_leading_up_2(&p5.clean());
    let (sp6w, c6w) = strip_leading_up_2(&p6.clean());

    println!("Converted {} to {} counted {c1w}", p1.display(), sp1w.display());
    println!("Converted {} to {} counted {c2w}", p2.display(), sp2w.display());
    println!("Converted {} to {} counted {c3w}", p3.display(), sp3w.display());
    println!("Converted {} to {} counted {c4w}", p4.display(), sp4w.display());
    println!("Converted {} to {} counted {c5w}", p5.display(), sp5w.display());
    println!("Converted {} to {} counted {c6w}", p6.display(), sp6w.display());
}

fn build_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut res = Vec::new();
    let mut wp = path;

    loop {
        let p = wp.parent();
        match p {
            None => break,
            Some(p) => {
                res.push(p.to_path_buf());
                wp = p;
            }
        }
    }
    res
}

fn print_ancestor_result(ancestors: &[PathBuf], start: &Path) -> () {
    println!("Start: {}", start.display());
    for (num, ancestor) in ancestors.iter().enumerate(){
        println!("{:02}: {}", num, ancestor.display())
    }
    println!();
}

/// Relative path of `..` components from `file`'s parent directory back to `dir`.
///
/// - `dir=/path/to/dir`, `file=/path/to/dir/sub/dir/file.txt` → `../../`
/// - `dir=/path/to/dir`, `file=/path/to/dir/file.txt` → `.`
///
/// Panics if `file` is not under `dir`.
fn relative_pardirs_to_dir(dir: &Path, file: &Path) -> PathBuf {
    debug_assert!(dir.is_absolute(), "dir path must be absolute");
    debug_assert!(file.is_absolute(), "file path must be absolute");
    debug_assert_eq!(dir.clean(), dir, "dir path must be cleaned");
    debug_assert_eq!(file.clean(), file, "dir path must be cleaned");
    assert!(
        file.starts_with(&dir),
        "provided path is not child of target path: {} is not under {}",
        file.display(),
        dir.display()
    );
    let parent = file
        .parent()
        .expect("file path must have a parent directory");
    let below = parent
        .strip_prefix(&dir)
        .expect("parent must be under dir after starts_with check");
    // INFO Check exists but it should already be guaranteed not to exist.
    // for c in below.components() {
    //     if !matches!(c, Component::Normal(_)) {
    //         assert!(false, "Absolute Path contained non-Normal intermediate entry.");
    //     }
    // }
    let depth = below
        .components()
        .count();
    if depth == 0 {
        PathBuf::from(".")
    } else {
        let mut up = String::with_capacity(depth * 3);
        for _ in 0..depth {
            up.push_str("../");
        }
        PathBuf::from(up)
    }
}

fn test_build_ancestor() -> () {
    let p1 = PathBuf::from("");
    let p2 = PathBuf::from("/");
    let p3 = PathBuf::from(".");
    let p4 = PathBuf::from("./a/b/c/d");
    let p5 = PathBuf::from("/home/user/Desktop/Folder");
    let p6 = PathBuf::from("../down/../other/../folder/../deep/dive/../other../..other/");

    print_ancestor_result(&build_ancestors(&p1), &p1);
    print_ancestor_result(&build_ancestors(&p2), &p2);
    print_ancestor_result(&build_ancestors(&p3), &p3);
    print_ancestor_result(&build_ancestors(&p4), &p4);
    print_ancestor_result(&build_ancestors(&p5), &p5);
    print_ancestor_result(&build_ancestors(&p6), &p6);

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
    test_strip();
    test_build_ancestor();
    let a = PathBuf::from("/path/to/base_dir/");
    let b = PathBuf::from("/path/to/base_dir");
    let c = PathBuf::from("/path/to/base");

    println!("'{}' starts with '{}'? {}", a.display(), b.display(), a.starts_with(&b));
    println!("'{}' starts with '{}'? {}", b.display(), a.display(), b.starts_with(&a));
    println!("'{}' starts with '{}'? {}", c.display(), b.display(), c.starts_with(&b));
    println!("'{}' starts with '{}'? {}", b.display(), c.display(), b.starts_with(&c));

    let target = PathBuf::from("/home/user/Desktop/TarExtract");
    let root = PathBuf::from("/");
    let base_child = PathBuf::from("/version.txt");

    // Panics!
    // let target_and_root = target.join(root.parent().unwrap());
    // println!("Joining {} with {}, expected {}", target.display(), root.display(), target_and_root.display());

    let target_and_root = target.join(base_child.parent().unwrap());
    println!("Parent is: {}", base_child.parent().unwrap().display());
    println!("Joining {} with {}, got {}, expected {}", target.display(), base_child.display(), target_and_root.display(), target.display());

    let base = Path::new("/path/to/dir");
    println!(
        "pardirs: {}",
        relative_pardirs_to_dir(base, Path::new("/path/to/dir/sub/dir/file.txt")).display()
    );
    println!(
        "pardirs: {}",
        relative_pardirs_to_dir(base, Path::new("/path/to/dir/file.txt")).display()
    );
    // relative_pardirs_to_dir(base, Path::new("/path/file.txt")); // panic: not a child
    println!(
        "pardirs: {}",
        relative_pardirs_to_dir(base, Path::new("/path/to/other/../dir/file.txt")).display()
    );
}