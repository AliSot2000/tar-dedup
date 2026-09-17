use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};
use std::borrow::Cow;

use std::{thread, time};


fn main() {
    let m = MultiProgress::new();
    let sty = ProgressStyle::with_template(
        "{prefix:<10} [{elapsed}] {bar:20.red/blue} {pos:>7}/{len:7} {msg}",
    )
        .unwrap()
        .progress_chars("##-");

    let total = m.add(ProgressBar::new(10));
    total.set_style(sty.clone());
    total.set_prefix("total");
    // Vec to hold handles
    let mut pbs = vec![];
    for i in 0..5 {
        let name = format!("Job #{i}");
        let pb = m.insert_before(
            &total,
            ProgressBar::new(3).with_finish(ProgressFinish::WithMessage(Cow::Borrowed("DONE!"))),
        );
        // Stash a handle to the pb to keep it alive till end of loop
        pbs.push(pb.clone());

        pb.set_style(sty.clone());
        pb.set_prefix(name);
        for _ in 0..3 {
            let ten_millis = time::Duration::from_millis(1000);
            thread::sleep(ten_millis);
            // Temporarily clear the screen so we can print a message to the terminal
            m.suspend(|| {
                eprintln!("from job #{i}...");
            });

            pb.inc(1);
        }
        pb.finish_using_style();
        total.inc(1);
    }

    total.finish();
}