use std::collections::HashMap;
use std::env;
use std::fs::{read_dir, read_to_string};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::time::Instant;

use once_cell::unsync::OnceCell;
use regex::{Captures, Regex, Replacer};

use zuban_python::{DiagnosticConfig, Project, ProjectOptions};

const USE_MYPY_TEST_FILES: [&str; 42] = [
    // Semanal tests
    //"semanal-abstractclasses.test",
    "semanal-basic.test",
    "semanal-classes.test",
    //"semanal-classvar.test",
    //"semanal-errors-python310.test",
    "semanal-errors.test",
    //"semanal-expressions.test",
    //"semanal-lambda.test",
    "semanal-literal.test",
    //"semanal-modules.test",
    "semanal-namedtuple.test",
    //"semanal-python310.test",
    "semanal-statements.test",
    //"semanal-symtable.test",
    "semanal-typealiases.test",
    //"semanal-typeddict.test",
    "semanal-typeinfo.test",
    "semanal-types.test",
    "check-semanal-error.test",
    "check-newsemanal.test",
    // Type checking tests
    "check-generics.test",
    "check-generic-alias.test",
    "check-typevar-unbound.test",
    "check-basic.test",
    "check-type-aliases.test",
    "check-typevar-values.test",
    "check-bound.test",
    "check-modules.test",
    //"check-modules-case.test",
    //"check-modules-fast.test",
    "check-functions.test",
    "check-varargs.test",
    "check-kwargs.test",
    "check-generic-subtyping.test",
    "check-classes.test",
    //"check-super.test",
    //"check-multiple-inheritance.test",
    //"check-classvar.test",
    "check-overloading.test",
    "check-literal.test",
    "check-unions.test",
    "check-union-or-syntax.test",
    //"check-protocols.test",
    //"check-callable.test",
    "check-parameter-specification.test",
    //"check-incremental.test",
    "check-expressions.test",
    //"check-statements.test",
    //"check-narrowing.test",
    //"check-isinstance.test",
    //"check-type-checks.test",
    "check-type-promotion.test",
    //"check-async-await.test",
    "check-inference.test",
    "check-inference-context.test",
    //"check-final.test",
    //"check-abstract.test",
    //"check-ignore.test",
    //"check-underscores.test",
    //"check-redefine.test",
    //"check-dynamic-typing.test",
    "check-selftype.test",
    "check-recursive-types.test",
    //"check-typeguard.test",
    "check-annotated.test",
    //"check-tuples.test",
    //"check-lists.test",
    //"check-enum.test",
    //"check-warnings.test",
    //"check-functools.test",
    //"check-singledispatch.test",
    //"check-ctypes.test",
    //"check-dataclasses.test",
    "check-namedtuple.test",
    "check-class-namedtuple.test",
    //"check-typeddict.test",
    "check-newtype.test",
    //"check-unsupported.test",
    //"check-attr.test",
    "check-optional.test",
    //"check-unreachable-code.test",
    //"check-possibly-undefined.test",
    //"check-slots.test",
    "check-typevar-tuple.test",
    //"check-dataclass-transform.test",
    //"fine-grained-dataclass-transform.test",
    //"check-newsyntax.test",
    //"check-fastparse.test",
    //"check-python38.test",
    //"check-python39.test",
    //"check-python310.test",
    //"check-python311.test",
    //"check-custom-plugin.test",
    "fine-grained.test",
    //"fine-grained-modules.test",
    //"fine-grained-suggest.test",
    //"fine-grained-follow-imports.test",
    //"fine-grained-blockers.test",
    //"fine-grained-cache-incremental.test",
    //"fine-grained-cycles.test",
    //"fine-grained-attr.test",
    //"fine-grained-inspect.test",

    //"check-columns.test",
    //"check-errorcodes.test",
    //"check-formatting.test",
    //"check-flags.test",
    //"check-serialize.test",
    //"cmdline.test",
    //"cmdline.pyproject.test",
    //"pep561.test",
    //"check-reports.test",
    //"check-inline-config.test",

    // Won't do, because it tests mypy internals
    //"check-incomplete-fixture.test",
    //"check-native-int.test",
];

const BASE_PATH: &str = "/mypylike/";

lazy_static::lazy_static! {
    static ref CASE: Regex = Regex::new(r"(?m)^\[case ([a-zA-Z_0-9-]+)\][ \t]*\n").unwrap();
    // This is how I found out about possible "commands in mypy, executed in
    // mypy/test-data/unit:
    // find . | grep check | xargs cat | grep '^\[' | grep -Ev '\[(out|case|file)'
    static ref CASE_PART: Regex = Regex::new(concat!(
        r"(?m)^\[(file|out\d*|builtins|typing|stale|rechecked|targets\d?|delete|triggered)",
        r"(?: ([^\]]*))?\][ \t]*\n"
    )).unwrap();
    static ref REPLACE_COMMENTS: Regex = Regex::new(r"(?m)^--.*$\n").unwrap();
    static ref REPLACE_TUPLE: Regex = Regex::new(r"\bTuple\b").unwrap();
    static ref REPLACE_MYPY: Regex = Regex::new(r"`-?\d+").unwrap();
    // Mypy has this weird distinction for literals like Literal[1]?
    static ref REPLACE_LITERAL_QUESTION_MARK: Regex = Regex::new(r"(Literal\[.*?\])\?").unwrap();
}

#[derive(Default, Clone, Debug)]
struct Step<'code> {
    deletions: Vec<&'code str>,
    files: HashMap<&'code str, &'code str>,
    out: &'code str,
}

#[derive(Debug)]
struct TestCase<'name, 'code> {
    file_name: &'name str,
    name: String,
    code: &'code str,
}

struct Steps<'code> {
    steps: Vec<Step<'code>>,
    flags: Vec<&'code str>,
}

impl<'name, 'code> TestCase<'name, 'code> {
    fn run(&self, projects: &mut HashMap<BaseConfig, LazyProject>, mypy_compatible_override: bool) {
        let steps = self.calculate_steps();
        let mut diagnostics_config = DiagnosticConfig::default();

        if steps.flags.contains(&"--ignore-missing-imports") {
            diagnostics_config.ignore_missing_imports = true;
        }
        let mut config = BaseConfig::default();
        if steps.flags.contains(&"--strict-optional") || self.file_name == "check-optional" {
            config.strict_optional = true;
        }
        if steps.flags.contains(&"--implicit-optional") {
            config.implicit_optional = true;
        }
        if mypy_compatible_override || steps.flags.contains(&"--mypy-compatible") {
            config.mypy_compatible = true;
        }
        let project = projects.get_mut(&config).unwrap();

        if steps
            .flags
            .iter()
            .position(|&r| r == "--python-version")
            .map(|p| ["2.7", "3.5", "3.6", "3.7", "3.8"].contains(&steps.flags[p + 1]))
            .unwrap_or(false)
        {
            // For now skip Python tests < 3.9, because it looks like we won't support them.
            println!("Skipped: {}, because {:?}", self.file_name, steps.flags);
            return;
        }

        for (i, step) in steps.steps.iter().enumerate() {
            if cfg!(feature = "zuban_debug") {
                println!(
                    "\nTest: {} ({}): Step {}/{}",
                    self.name,
                    self.file_name,
                    i + 1,
                    steps.steps.len()
                );
            }
            let mut wanted = wanted_output(project, step);

            for path in &step.deletions {
                #[allow(unused_must_use)]
                {
                    project.unload_in_memory_file(&(BASE_PATH.to_owned() + path));
                }
            }
            let diagnostics: Vec<_> = project
                .diagnostics(&diagnostics_config)
                .iter()
                .map(|d| d.as_string())
                .collect();

            let actual = replace_annoyances(diagnostics.join("\n"));
            let mut actual_lines = actual
                .trim()
                .split('\n')
                .map(|s| s.to_lowercase())
                .filter_map(temporarily_skip)
                .collect::<Vec<_>>();
            if actual_lines == [""] {
                actual_lines.pop();
            }
            actual_lines.sort();

            // For now we want to compare lower cases, because mypy mixes up list[] and List[]
            let mut wanted_lower: Vec<_> = wanted
                .iter()
                .map(|s| s.to_lowercase())
                .filter_map(temporarily_skip)
                .collect();
            wanted_lower.sort();

            // To check output only sort by filenames, which should be enough.
            wanted.sort_by_key(|line| line.split(':').next().unwrap().to_owned());

            assert_eq!(
                actual_lines,
                wanted_lower,
                "\n\nError in {} ({}): Step {}/{}\n\nWanted:\n{}\n\nActual:\n{}\n",
                &self.name,
                self.file_name,
                i + 1,
                steps.steps.len(),
                wanted.iter().fold(String::new(), |a, b| a + b + "\n"),
                actual,
            );
        }
        for step in &steps.steps {
            for path in step.files.keys() {
                #[allow(unused_must_use)]
                {
                    project.unload_in_memory_file(&(BASE_PATH.to_owned() + path));
                }
            }
        }
    }

    fn calculate_steps(&self) -> Steps {
        let mut steps = HashMap::<usize, Step>::new();
        steps.insert(1, Default::default());
        let mut current_step_index = 1;
        let mut current_type = "file";
        let mut current_rest = "__main__";
        let mut current_step_start = 0;
        let mut flags = vec![];

        let mut process_step_part2 = |step_index, type_, in_between, rest: &'code str| {
            let step = if let Some(s) = steps.get_mut(&step_index) {
                s
            } else {
                steps.insert(step_index, Default::default());
                steps.get_mut(&step_index).unwrap()
            };
            if type_ == "file" {
                step.files.insert(rest, in_between);
            } else if type_ == "out" {
                if !(self.file_name.contains("semanal-")
                    && (in_between.starts_with("MypyFile:1")
                        || in_between.starts_with("TypeInfoMap(")))
                {
                    // Semanal files print the AST in success cases. We only care about the
                    // errors, because zuban's tree is probably different. We still test however
                    // that there are no errors in those cases.
                    step.out = in_between;
                }
            } else if type_ == "delete" {
                step.deletions.push(rest)
            }
        };

        let mut process_step = |step_index, type_, step_start, step_end, rest: &'code str| {
            let in_between = &self.code[step_start..step_end];

            if type_ == "out" && step_index == 1 {
                // For now just ignore different versions and overwrite the out. This works,
                // because we always target the latest version and older versions are currently
                // listed below newer ones (by convention?).
                if !rest.starts_with("version>=") && rest != "skip-path-normalization" {
                    assert_eq!(rest, "");
                }
                for (i, part) in in_between.split("==\n").enumerate() {
                    process_step_part2(i + 1, "out", part, rest)
                }
            } else {
                process_step_part2(step_index, type_, in_between, rest)
            }
            if rest == "__main__" && in_between.starts_with("# flags: ") {
                let all_flags = &in_between[9..in_between.find('\n').unwrap()];
                flags = all_flags.split(' ').collect();
            }
        };

        for capture in CASE_PART.captures_iter(self.code) {
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
            self.code.len(),
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
}

fn replace_annoyances(s: String) -> String {
    s.replace("builtins.", "")
}

fn temporarily_skip(s: String) -> Option<String> {
    if s.ends_with(" overlap with incompatible return types")
        && s.contains("overloaded function signatures")
    {
        return None;
    }
    Some(s)
}

fn wanted_output(project: &mut Project, step: &Step) -> Vec<String> {
    let mut wanted = step
        .out
        .trim()
        .split('\n')
        .filter_map(cleanup_mypy_issues)
        .collect::<Vec<_>>();

    if wanted == [""] {
        wanted.pop();
    }

    let mut sorted_files: Vec<_> = step.files.iter().collect();
    sorted_files.sort();
    for (&path, &code) in &sorted_files {
        let p = if path == "__main__" {
            // TODO this if is so weird. Why is this shit needed???
            "main"
        } else {
            path
        };
        let lines: Box<_> = code.split('\n').collect();
        for (line_nr, type_, comment) in ErrorCommentsOnCode(&lines, lines.iter().enumerate()) {
            for comment in comment.split(" # E: ") {
                for (i, comment) in comment.split(" # N: ").enumerate() {
                    if i == 0 {
                        wanted.push(format!("{p}:{line_nr}: {type_}: {}", comment.trim_end()))
                    } else {
                        wanted.push(format!("{p}:{line_nr}: note: {}", comment.trim_end()))
                    }
                }
            }
        }
        project.load_in_memory_file(BASE_PATH.to_owned() + path, code.to_owned());
    }
    for line in &mut wanted {
        replace_unions(line)
    }
    wanted
        .into_iter()
        .map(|line| {
            REPLACE_LITERAL_QUESTION_MARK
                .replace_all(&line, r"$1")
                .into()
        })
        .collect()
}

fn replace_unions(line: &mut String) {
    while let Some(index) = line.rfind("Union[") {
        let mut brackets = 0;
        let mut commas = vec![];
        let mut end = 0;
        for (i, chr) in line[index..].char_indices() {
            match chr {
                '[' => brackets += 1,
                ']' => {
                    brackets -= 1;
                    if brackets == 0 {
                        end = i;
                        break;
                    }
                }
                ',' => {
                    if brackets == 1 {
                        commas.push(i);
                    }
                }
                _ => (),
            }
        }
        assert_eq!(brackets, 0);
        assert_ne!(end, 0);
        line.replace_range(index + end..index + end + 1, "");
        for i in commas.iter().rev() {
            line.replace_range(index + i..index + i + 1, " |");
        }
        line.replace_range(index..index + "Union[".len(), "");
    }
}

struct ErrorCommentsOnCode<'a>(
    &'a [&'a str],
    std::iter::Enumerate<std::slice::Iter<'a, &'a str>>,
);

impl Iterator for ErrorCommentsOnCode<'_> {
    type Item = (usize, &'static str, String);
    fn next(&mut self) -> Option<Self::Item> {
        for (i, line) in &mut self.1 {
            let was_exception = line.find("# E: ");
            if let Some(pos) = was_exception.or_else(|| line.find("# N: ")) {
                let mut backslashes = 0;
                for i in (0..i).rev() {
                    if !self.0[i].ends_with('\\') {
                        break;
                    }
                    backslashes += 1;
                }
                if let Some(out) = cleanup_mypy_issues(&line[pos + 5..]) {
                    return Some((
                        i + 1 - backslashes,
                        match was_exception {
                            Some(_) => "error",
                            None => "note",
                        },
                        out,
                    ));
                }
            }
        }
        None
    }
}
fn cleanup_mypy_issues(mut s: &str) -> Option<String> {
    if s.contains("See https://mypy.readthedocs.io/en/stable/running_mypy.html#missing-imports") {
        return None;
    }
    if s.contains("\" defined here") {
        // TODO we might not want to skip this note in the future.
        return None;
    }
    if s.ends_with(" \\") {
        s = &s[..s.len() - 2];
    }
    let s = REPLACE_TUPLE.replace_all(s, TypeStuffReplacer());
    let s = REPLACE_MYPY.replace_all(&s, "");
    Some(replace_annoyances(s.replace("tmp/", "")))
}

struct TypeStuffReplacer();

impl Replacer for TypeStuffReplacer {
    fn replace_append(&mut self, _caps: &Captures<'_>, dst: &mut String) {
        if dst.ends_with("(got \"")
            || dst.ends_with(", expected \"")
            || dst.ends_with("has type \"")
        {
            dst.push_str("tuple")
        } else {
            dst.push_str("builtins.tuple")
        }
    }
}

fn calculate_filters(args: Vec<String>) -> Vec<String> {
    let mut filters = vec![];
    for s in args.into_iter().skip(1) {
        if s != "mypy" {
            filters.push(s)
        }
    }
    filters
}

#[derive(PartialEq, Eq, Hash, Default, Copy, Clone)]
struct BaseConfig {
    strict_optional: bool,
    implicit_optional: bool,
    mypy_compatible: bool,
}

struct LazyProject {
    project: OnceCell<Project>,
    options: ProjectOptions,
}

impl LazyProject {
    fn new(options: ProjectOptions) -> LazyProject {
        LazyProject {
            project: OnceCell::new(),
            options,
        }
    }

    fn init(&self) -> &Project {
        self.project
            .get_or_init(|| Project::new(self.options.clone()))
    }
}

impl Deref for LazyProject {
    type Target = Project;

    fn deref(&self) -> &Self::Target {
        self.init()
    }
}

impl DerefMut for LazyProject {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.init();
        self.project.get_mut().unwrap()
    }
}

fn main() {
    let cli_args: Vec<String> = env::args().collect();
    let filters = calculate_filters(cli_args);

    let mut projects = HashMap::new();
    for strict_optional in [false, true] {
        for implicit_optional in [false, true] {
            for mypy_compatible in [false, true] {
                let config = BaseConfig {
                    strict_optional,
                    implicit_optional,
                    mypy_compatible,
                };
                projects.insert(
                    config,
                    LazyProject::new(ProjectOptions {
                        path: BASE_PATH.to_owned(),
                        implicit_optional: config.implicit_optional,
                        strict_optional: config.strict_optional,
                        mypy_compatible: config.mypy_compatible,
                    }),
                );
            }
        }
    }

    let skipped = skipped();

    let files = find_mypy_style_files();
    let start = Instant::now();
    let mut full_count = 0;
    let mut ran_count = 0;
    let file_count = files.len();
    for (from_mypy_test_suite, file) in files {
        let code = read_to_string(&file).unwrap();
        let code = REPLACE_COMMENTS.replace_all(&code, "");
        let stem = file.file_stem().unwrap().to_owned();
        let file_name = stem.to_str().unwrap();
        for case in mypy_style_cases(file_name, &code) {
            full_count += 1;
            if !filters.is_empty() && !filters.contains(&case.name) {
                continue;
            }
            if skipped.iter().any(|s| s.is_skip(&case.name)) {
                println!("Skipped: {}", case.name);
                continue;
            }
            if !from_mypy_test_suite {
                // Run our own tests both with mypy-compatible and without it.
                case.run(&mut projects, from_mypy_test_suite);
                ran_count += 1;
            }
            case.run(&mut projects, from_mypy_test_suite);
            ran_count += 1;
        }
    }
    println!(
        "Ran {} of {} mypy-like tests in {} files; finished in {:.2}s",
        ran_count,
        full_count,
        file_count,
        start.elapsed().as_secs_f32(),
    );
}

fn mypy_style_cases<'a, 'b>(file_name: &'a str, code: &'b str) -> Vec<TestCase<'a, 'b>> {
    let mut cases = vec![];

    let mut add = |name, start, end| {
        cases.push(TestCase {
            file_name,
            name,
            code: &code[start..end],
        });
    };

    let mut start = None;
    let mut next_name = None;
    for capture in CASE.captures_iter(code) {
        if let Some(start) = start {
            add(
                next_name.take().unwrap(),
                start,
                capture.get(0).unwrap().start(),
            );
        }
        next_name = Some(capture.get(1).unwrap().as_str().to_owned());
        start = Some(capture.get(0).unwrap().end())
    }

    add(
        next_name.unwrap_or_else(|| panic!("File without test cases: {:?}", file_name)),
        start.unwrap(),
        code.len(),
    );
    cases
}

fn get_base() -> PathBuf {
    // TODO windows, this slash probably makes problems...
    let mut base = PathBuf::from(file!().replace("zuban_python/", ""));
    assert!(base.pop());
    base
}

fn find_mypy_style_files() -> Vec<(bool, PathBuf)> {
    let base = get_base();
    let mut entries = vec![];

    // Include local tests
    let mut path = base.clone();
    path.push("tests");

    let mut our_own_tests: Vec<_> = read_dir(path)
        .unwrap()
        .map(|res| (false, res.unwrap().path()))
        .collect();

    our_own_tests.sort();

    // Include mypy tests
    for name in USE_MYPY_TEST_FILES {
        let mut path = base.clone();
        path.extend(["mypy", "test-data", "unit", name]);
        entries.push((true, path));
    }

    entries.extend(our_own_tests);
    entries
}

#[derive(Debug)]
struct Skipped {
    name: String,
    start_star: bool,
    end_star: bool,
}

impl Skipped {
    fn is_skip(&self, name: &str) -> bool {
        if self.start_star && self.end_star {
            name.contains(&self.name)
        } else if self.start_star {
            name.ends_with(&self.name)
        } else if self.end_star {
            name.starts_with(&self.name)
        } else {
            self.name == name
        }
    }
}

fn skipped() -> Box<[Skipped]> {
    let mut skipped_path = get_base();
    skipped_path.push("skipped");
    let file = read_to_string(skipped_path).unwrap();

    file.trim()
        .split('\n')
        .map(|mut x| {
            let start_star = x.starts_with('*');
            let end_star = x.ends_with('*');
            if start_star {
                x = &x[1..];
            }
            if end_star {
                x = &x[..x.len() - 1]
            }
            Skipped {
                name: x.to_owned(),
                start_star,
                end_star,
            }
        })
        .collect()
}
