mod testdir;

use std::{collections::HashMap, sync::Arc};

use regex::Regex;

pub use testdir::TestDir;
use utils::dedent;
use vfs::{LocalFS, NormalizedPath};

lazy_static::lazy_static! {
    // This is how I found out about possible "commands in mypy, executed in
    // mypy/test-data/unit:
    // find . | grep check | xargs cat | grep '^\[' | grep -Ev '\[(out|case|file)'
    static ref CASE_PART: Regex = Regex::new(concat!(
        r"(?m)^\[(file|out\d*|builtins|typing|stale\d*|rechecked|targets\d?|delete|triggered|fixture)",
        r"(?: ([^\]]*))?\][ \t]*\r?\n"
    )).unwrap();
    static ref SPLIT_OUT: Regex = Regex::new(r"(\n|^)==").unwrap();
}

pub struct Steps<'code> {
    pub steps: Vec<Step<'code>>,
    pub flags: Vec<&'code str>,
}

#[derive(Default, Clone, Debug)]
pub struct Step<'code> {
    pub deletions: Vec<&'code str>,
    pub files: HashMap<&'code str, &'code str>,
    pub out: &'code str,
}

pub fn calculate_steps<'code>(file_name: Option<&str>, code: &'code str) -> Steps<'code> {
    let file_name = file_name.unwrap_or("We don't know the name but it shouldn't matter");
    let mut steps = HashMap::<usize, Step>::new();
    steps.insert(1, Default::default());
    let mut current_step_index = 1;
    let mut current_type = "file";
    let mut current_rest = "__main__.py";
    let mut current_step_start = 0;
    let mut flags = vec![];

    let mut process_step_part2 = |step_index, type_, in_between, rest: &'code str| {
        let step = if let Some(s) = steps.get_mut(&step_index) {
            s
        } else {
            steps.insert(step_index, Default::default());
            steps.get_mut(&step_index).unwrap()
        };
        if type_ == "file" || type_ == "fixture" {
            step.files.insert(rest, in_between);
        } else if type_ == "out" {
            if !((file_name.contains("semanal-") || file_name.starts_with("parse"))
                && (in_between.starts_with("MypyFile:1") || in_between.starts_with("TypeInfoMap(")))
            {
                // Semanal files print the AST in success cases. We only care about the
                // errors, because zuban's tree is probably different. We still test however
                // that there are no errors in those cases.
                step.out = in_between;
            }
            if file_name.starts_with("pythoneval") && !in_between.contains(".py:") {
                // pythoneval.test and pythoneval-asyncio.test
                step.out = "";
            }
        } else if type_ == "delete" {
            step.deletions.push(rest)
        }
    };

    let mut process_step = |step_index, type_, step_start, step_end, rest: &'code str| {
        let in_between = &code[step_start..step_end];

        if type_ == "out" && step_index == 1 && !file_name.contains("semanal-") {
            // For now just ignore different versions and overwrite the out. This works,
            // because we always target the latest version and older versions are currently
            // listed below newer ones (by convention?).
            if !rest.starts_with("version>=")
                && !rest.starts_with("version==")
                && rest != "skip-path-normalization"
            {
                assert_eq!(rest, "");
            }
            for (i, part) in SPLIT_OUT.split(in_between).enumerate() {
                process_step_part2(i + 1, "out", part, rest)
            }
        } else {
            process_step_part2(step_index, type_, in_between, rest)
        }
        if rest == "__main__.py" {
            if let Some(flags_str) = find_flags(in_between) {
                flags = flags_str.split(' ').collect();
            }
        }
    };

    for capture in CASE_PART.captures_iter(code) {
        process_step(
            current_step_index,
            current_type,
            current_step_start,
            capture.get(0).unwrap().start(),
            current_rest,
        );

        current_type = capture.get(1).unwrap().as_str();
        current_rest = capture.get(2).map(|x| x.as_str()).unwrap_or("");
        current_step_start = capture.get(0).unwrap().end();

        current_step_index = 1;
        if current_type == "file" || current_type == "delete" {
            let last = current_rest.chars().last().unwrap();
            if let Some(digit) = last.to_digit(10) {
                current_step_index = digit as usize;
                current_rest = &current_rest[..current_rest.len() - 2];
            }
        } else if current_type.starts_with("out") && current_type.len() > 3 {
            if let Some(digit) = current_type.chars().nth(3).unwrap().to_digit(10) {
                current_step_index = digit as usize;
                current_type = "out";
            }
        }
    }
    process_step(
        current_step_index,
        current_type,
        current_step_start,
        code.len(),
        current_rest,
    );

    let mut result_steps = vec![];
    for i in 1..steps.len() + 1 {
        result_steps.push(steps[&i].clone());
    }
    Steps {
        steps: result_steps,
        flags,
    }
}

fn find_flags(string: &str) -> Option<&str> {
    for line in string.split('\n') {
        if !line.starts_with('#') {
            break;
        }
        if let Some(flags) = line.strip_prefix("# flags: ") {
            return Some(flags.trim_end_matches('\r'));
        }
    }
    None
}

pub fn typeshed_path() -> Arc<NormalizedPath> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let typeshed_path = p.ancestors().nth(2).unwrap().join("typeshed");
    LocalFS::without_watcher().normalized_path_from_current_dir(typeshed_path.to_str().unwrap())
}

pub fn write_files_from_fixture(fixture: &str, root_dir_contains_symlink: bool) -> TestDir {
    let tmp_dir = TestDir::new(root_dir_contains_symlink);
    let dedented_fixture = dedent(fixture);
    for entry in fixture_to_file_entry(&dedented_fixture) {
        tmp_dir.write_file(entry.path, entry.text)
    }
    tmp_dir
}

struct FileEntry<'x> {
    path: &'x str,
    text: &'x str,
}

fn fixture_to_file_entry(fixture: &str) -> impl Iterator<Item = FileEntry> {
    let steps = calculate_steps(None, fixture);
    assert!(
        steps.flags.is_empty(),
        "For now flags in fixtures are not supported"
    );
    assert_eq!(steps.steps.len(), 1, "For now we only support one step");
    let mut first = steps.steps.into_iter().next().unwrap();
    assert!(first.out.is_empty());
    assert!(first.deletions.is_empty());
    let main_content = first
        .files
        .remove("__main__.py")
        .expect("Should always be there");
    assert_eq!(main_content.trim(), "");
    first
        .files
        .into_iter()
        .map(|(path, text)| FileEntry { path, text })
}
