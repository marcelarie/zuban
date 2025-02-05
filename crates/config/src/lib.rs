use std::borrow::Cow;

use anyhow::bail;
use ini::{Ini, ParseOption};
use regex::Regex;
use toml_edit::{DocumentMut, Item, Table, Value};

type ConfigResult = anyhow::Result<bool>;

const OPTIONS_STARTING_WITH_ALLOW: [&str; 3] = [
    "allow_untyped_globals",
    "allow_redefinition",
    "allow_empty_bodies",
];

#[derive(Default)]
pub struct DiagnosticConfig {
    pub show_error_codes: bool,
    pub show_error_end: bool,
    pub show_column_numbers: bool,
}

#[derive(Clone, Default)]
pub struct ProjectOptions {
    pub settings: Settings,
    pub flags: TypeCheckerFlags,
    pub overrides: Vec<OverrideConfig>,
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Settings {
    pub platform: Option<String>,
    pub python_version: PythonVersion,
    pub python_executable: Option<String>,
    pub mypy_path: Vec<String>,
    pub prepended_site_packages: Vec<String>,
    pub mypy_compatible: bool,
    pub files_or_directories_to_check: Vec<String>,
    pub typeshed_path: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            platform: None,
            python_version: PythonVersion::new(3, 13),
            python_executable: None,
            typeshed_path: std::env::var("ZUBAN_TYPESHED").ok(),
            mypy_path: vec![],
            mypy_compatible: false,
            files_or_directories_to_check: vec![],
            prepended_site_packages: vec![],
        }
    }
}

impl Settings {
    pub fn computed_platform(&self) -> &str {
        self.platform.as_deref().unwrap_or("posix")
    }
}

impl ProjectOptions {
    pub fn new(settings: Settings, flags: TypeCheckerFlags) -> Self {
        Self {
            settings,
            flags,
            overrides: vec![],
        }
    }

    pub fn from_mypy_ini(
        code: &str,
        diagnostic_config: &mut DiagnosticConfig,
    ) -> anyhow::Result<Self> {
        let options = ParseOption {
            indented_multiline_values: true,
            ..Default::default()
        };
        let ini = Ini::load_from_str_opt(code, options)?;
        let mut flags = TypeCheckerFlags::default();
        let mut settings = Settings::default();
        let mut overrides = vec![];
        for (name, section) in ini.iter() {
            let Some(name) = name else { continue };
            if name == "mypy" {
                for (key, value) in section.iter() {
                    apply_from_base_config(
                        &mut settings,
                        &mut flags,
                        diagnostic_config,
                        key,
                        IniOrTomlValue::Ini(value),
                    )?;
                }
            } else if let Some(rest) = name.strip_prefix("mypy-") {
                overrides.push(OverrideConfig {
                    module: rest.into(),
                    config: section
                        .iter()
                        .map(|(x, y)| (x.into(), OverrideIniOrTomlValue::Ini(y.into())))
                        .collect(),
                })
            }
        }
        Ok(ProjectOptions {
            settings,
            flags,
            overrides,
        })
    }

    pub fn from_pyproject_toml(
        code: &str,
        diagnostic_config: &mut DiagnosticConfig,
    ) -> anyhow::Result<Self> {
        let document = code.parse::<DocumentMut>()?;
        let mut flags = TypeCheckerFlags::default();
        let mut settings = Settings::default();
        if let Some(config) = document.get("tool").and_then(|item| item.get("mypy")) {
            let Item::Table(table) = config else {
                bail!("Expected tool.mypy to be a table in pyproject.toml");
            };

            let mut overrides = vec![];
            for (key, item) in table.iter() {
                match item {
                    Item::Value(value) => {
                        apply_from_base_config(
                            &mut settings,
                            &mut flags,
                            diagnostic_config,
                            key,
                            IniOrTomlValue::Toml(value),
                        )?;
                    }
                    Item::ArrayOfTables(override_tables) if key == "overrides" => {
                        for override_table in override_tables.iter() {
                            for module in pyproject_toml_override_module_names(override_table)? {
                                let mut config = vec![];
                                for (key, part) in override_table.iter() {
                                    if key != "module" {
                                        match part {
                                            Item::Value(v) => config.push((
                                                key.into(),
                                                OverrideIniOrTomlValue::Toml(v.clone()),
                                            )),
                                            _ => {
                                                bail!("Found unexpected value in override in pyproject.toml".to_string())
                                            }
                                        }
                                    }
                                }
                                overrides.push(OverrideConfig { module, config })
                            }
                        }
                    }
                    Item::None | Item::Table(_) | Item::ArrayOfTables(_) => {
                        bail!("Expected tool.mypy to be simple table in pyproject.toml");
                    }
                }
            }
            Ok(ProjectOptions {
                settings,
                flags,
                overrides,
            })
        } else {
            Ok(ProjectOptions::default())
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TypeCheckerFlags {
    pub strict_optional: bool,
    pub strict_equality: bool,
    pub implicit_optional: bool,
    pub check_untyped_defs: bool,
    pub ignore_missing_imports: bool,

    pub disallow_untyped_defs: bool,
    pub disallow_untyped_calls: bool,
    pub disallow_untyped_decorators: bool,
    pub disallow_any_generics: bool,
    pub disallow_any_decorated: bool,
    pub disallow_any_explicit: bool,
    pub disallow_any_unimported: bool,
    pub disallow_any_expr: bool,
    pub disallow_subclassing_any: bool,
    pub disallow_incomplete_defs: bool,
    pub allow_untyped_globals: bool,
    pub allow_empty_bodies: bool,
    pub warn_unreachable: bool,
    pub warn_redundant_casts: bool,
    pub warn_return_any: bool,
    pub warn_no_return: bool,
    pub local_partial_types: bool,
    pub no_implicit_reexport: bool,
    pub disable_bytearray_promotion: bool,
    pub disable_memoryview_promotion: bool,

    pub enabled_error_codes: Vec<String>,
    pub disabled_error_codes: Vec<String>,
    pub always_true_symbols: Vec<String>,
    pub always_false_symbols: Vec<String>,
    pub excludes: Vec<ExcludeRegex>,

    pub extra_checks: bool,
    pub case_sensitive: bool,
}

impl Default for TypeCheckerFlags {
    fn default() -> Self {
        Self {
            strict_optional: true,
            strict_equality: false,
            implicit_optional: false,
            check_untyped_defs: false,
            ignore_missing_imports: false,
            disallow_untyped_defs: false,
            disallow_untyped_calls: false,
            disallow_untyped_decorators: false,
            disallow_any_generics: false,
            disallow_any_decorated: false,
            disallow_any_explicit: false,
            disallow_any_unimported: false,
            disallow_any_expr: false,
            disallow_subclassing_any: false,
            disallow_incomplete_defs: false,
            allow_untyped_globals: false,
            allow_empty_bodies: false,
            warn_unreachable: false,
            warn_redundant_casts: false,
            warn_return_any: false,
            warn_no_return: true,
            local_partial_types: false,
            no_implicit_reexport: false,
            disable_bytearray_promotion: false,
            disable_memoryview_promotion: false,
            excludes: vec![],
            always_true_symbols: vec![],
            always_false_symbols: vec![],
            enabled_error_codes: vec![],
            disabled_error_codes: vec![],
            extra_checks: false,
            case_sensitive: true,
        }
    }
}

impl TypeCheckerFlags {
    pub fn enable_all_strict_flags(&mut self) {
        // Use for --strict
        // self.warn_unused_configs = true;
        self.disallow_any_generics = true;
        self.disallow_subclassing_any = true;
        self.disallow_untyped_calls = true;
        self.disallow_untyped_defs = true;
        self.disallow_incomplete_defs = true;
        self.check_untyped_defs = true;
        self.disallow_untyped_decorators = true;
        self.warn_redundant_casts = true;
        // self.warn_unused_ignores = true;
        self.warn_return_any = true;
        self.no_implicit_reexport = true;
        self.strict_equality = true;
        self.extra_checks = true;
    }

    pub fn enable_strict_bytes(&mut self) {
        self.disable_bytearray_promotion = true;
        self.disable_memoryview_promotion = true;
    }
}

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Debug)]
pub struct PythonVersion {
    pub major: usize,
    pub minor: usize,
}

impl PythonVersion {
    pub fn new(major: usize, minor: usize) -> Self {
        Self { major, minor }
    }

    pub fn at_least_3_dot(&self, minor: usize) -> bool {
        self.major >= 3 && self.minor >= minor
    }
}

impl std::str::FromStr for PythonVersion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = "Expected a dot separated python version like 3.13";
        let Some((major, minor)) = s.split_once(".") else {
            return Err(error.into());
        };
        Ok(Self {
            major: major.parse().map_err(|i| format!("{error} ({i})"))?,
            minor: minor.parse().map_err(|i| format!("{error} ({i})"))?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ExcludeRegex {
    pub regex_str: String,
    pub regex: Regex,
}

impl ExcludeRegex {
    pub fn new(regex_str: String) -> Result<Self, regex::Error> {
        let regex = Regex::new(&regex_str)?;
        Ok(Self { regex_str, regex })
    }
}

impl std::cmp::PartialEq for ExcludeRegex {
    fn eq(&self, other: &Self) -> bool {
        self.regex_str == other.regex_str
    }
}
impl std::cmp::Eq for ExcludeRegex {}
impl std::hash::Hash for ExcludeRegex {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.regex_str.hash(state)
    }
}

#[derive(Clone)]
pub struct OverridePath {
    pub path: Vec<Box<str>>,
    pub star: bool, // For things like foo.bar.*
}

impl From<&str> for OverridePath {
    fn from(mut value: &str) -> Self {
        let mut star = false;
        if let Some(new_s) = value.strip_suffix(".*") {
            value = new_s;
            star = true;
        }
        OverridePath {
            path: value.split('.').map(|s| s.into()).collect(),
            star,
        }
    }
}

#[derive(Clone)]
enum OverrideIniOrTomlValue {
    Toml(Value),
    Ini(Box<str>),
}

#[derive(Clone)]
pub struct OverrideConfig {
    pub module: OverridePath, // Path like foo.bar or foo.bar.*
    // Key/Value mappings
    config: Vec<(Box<str>, OverrideIniOrTomlValue)>,
}

impl OverrideConfig {
    pub fn apply_to_flags_and_return_ignore_errors(
        &self,
        flags: &mut TypeCheckerFlags,
    ) -> ConfigResult {
        let mut ignore_errors = false;
        for (key, value) in self.config.iter() {
            ignore_errors |= apply_from_config_part(
                flags,
                key,
                match value {
                    OverrideIniOrTomlValue::Toml(v) => IniOrTomlValue::Toml(v),
                    OverrideIniOrTomlValue::Ini(v) => IniOrTomlValue::Ini(v),
                },
            )?;
        }
        Ok(ignore_errors)
    }
}

fn pyproject_toml_override_module_names(table: &Table) -> anyhow::Result<Vec<OverridePath>> {
    match table.get("module") {
        Some(Item::Value(Value::String(s))) => Ok(vec![s.value().as_str().into()]),
        Some(Item::Value(Value::Array(list))) => {
            let mut result = vec![];
            for entry in list {
                match entry {
                    Value::String(s) => result.push(s.value().as_str().into()),
                    _ => bail!("TODO find an error name here"),
                }
            }
            Ok(result)
        }
        Some(_) => bail!("Unexpected value for module in override in pyproject.toml"),
        None => bail!("Expected a module entry for every override in pyproject.toml"),
    }
}

#[derive(Debug, Copy, Clone)]
pub enum IniOrTomlValue<'config> {
    Toml(&'config Value),
    Ini(&'config str),
    InlineConfigNoValue,
}

impl IniOrTomlValue<'_> {
    fn as_repr(&self) -> Cow<str> {
        match self {
            Self::Toml(v) => Cow::from(v.to_string()),
            Self::Ini(v) => Cow::Borrowed(v),
            Self::InlineConfigNoValue => Cow::Borrowed("True"),
        }
    }

    fn as_bool(&self, invert: bool) -> anyhow::Result<bool> {
        let result = match self {
            Self::Toml(v) => v
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Expected bool, got {}", v.to_string().trim()))?,
            Self::Ini(value) => match value.to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => bail!("Expected bool, got \"{value}\""),
            },
            Self::InlineConfigNoValue => true,
        };
        Ok(result != invert)
    }

    fn as_str_list(&self, key: &str, split_on: &[char]) -> anyhow::Result<Vec<String>> {
        let split_str = |s| split_and_trim(s, split_on).map(|x| x.to_string()).collect();
        match self {
            Self::Toml(v) => {
                if let Some(s) = v.as_str() {
                    return Ok(split_str(s));
                }
                v.as_array()
                    .ok_or_else(|| anyhow::anyhow!("Expected an array or string for {key}"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| anyhow::anyhow!("TODO error"))
                    })
                    .collect()
            }
            Self::Ini(s) => Ok(split_str(s)),
            Self::InlineConfigNoValue => unreachable!(),
        }
    }
}

fn maybe_invert(name: &str) -> (bool, Cow<str>) {
    if let Some(after_no_prefix) = name.strip_prefix("no_") {
        return (true, Cow::Borrowed(after_no_prefix));
    } else if name.starts_with("allow") && !OPTIONS_STARTING_WITH_ALLOW.contains(&name) {
        return (true, Cow::Owned(format!("dis{name}")));
    } else if let Some(after_dis_prefix) = name.strip_prefix("dis") {
        if OPTIONS_STARTING_WITH_ALLOW.contains(&after_dis_prefix) {
            return (true, Cow::Borrowed(after_dis_prefix));
        }
    }
    (false, Cow::Borrowed(name))
}

pub fn set_flag_and_return_ignore_errors(
    flags: &mut TypeCheckerFlags,
    name: &str,
    value: IniOrTomlValue,
) -> ConfigResult {
    let (invert, option_name) = maybe_invert(name);
    let add_list_of_str = |target: &mut Vec<String>| {
        if invert {
            bail!("Can not invert non-boolean key {option_name}")
        } else {
            match &value {
                IniOrTomlValue::Toml(Value::Array(lst)) => {
                    for entry in lst.iter() {
                        match entry {
                            Value::String(s) => target.push(s.value().clone()),
                            _ => bail!("TODO expected string array for {name}"),
                        }
                    }
                    Ok(false)
                }
                IniOrTomlValue::Toml(Value::String(s)) => {
                    // Apparently Mypy allows single strings for things like
                    //
                    //     enable_error_code = "ignore-without-code"
                    target.push(s.value().clone());
                    Ok(false)
                }
                IniOrTomlValue::Ini(v) => {
                    target.extend(split_and_trim(v, &[',']).map(String::from));
                    Ok(false)
                }
                _ => bail!("TODO expected string for {name}"),
            }
        }
    };
    match option_name.as_ref() {
        "exclude" => {
            if invert {
                bail!("Can not invert non-boolean key {option_name}")
            }
            add_excludes(&mut flags.excludes, value)
        }
        "always_true" => add_list_of_str(&mut flags.always_true_symbols),
        "always_false" => add_list_of_str(&mut flags.always_false_symbols),
        "enable_error_code" => add_list_of_str(&mut flags.enabled_error_codes),
        "disable_error_code" => add_list_of_str(&mut flags.disabled_error_codes),
        "strict" => bail!(concat!(
            r#"Setting "strict" not supported in inline configuration: "#,
            r#"specify it in a configuration file instead, or set individual "#,
            r#"inline flags (see "mypy -h" for the list of flags enabled in strict mode)"#
        )),
        _ => set_bool_init_flags(flags, name, &option_name, value, invert),
    }
}

fn set_bool_init_flags(
    flags: &mut TypeCheckerFlags,
    original_name: &str,
    name: &str,
    value: IniOrTomlValue,
    invert: bool,
) -> ConfigResult {
    match name {
        "strict_optional" => flags.strict_optional = value.as_bool(invert)?,
        "strict_equality" => flags.strict_equality = value.as_bool(invert)?,
        "implicit_optional" => flags.implicit_optional = value.as_bool(invert)?,
        "check_untyped_defs" => flags.check_untyped_defs = value.as_bool(invert)?,
        "ignore_missing_imports" => flags.ignore_missing_imports = value.as_bool(invert)?,

        "disallow_untyped_defs" => flags.disallow_untyped_defs = value.as_bool(invert)?,
        "disallow_untyped_calls" => flags.disallow_untyped_calls = value.as_bool(invert)?,
        "disallow_untyped_decorators" => {
            flags.disallow_untyped_decorators = value.as_bool(invert)?
        }
        "disallow_any_generics" => flags.disallow_any_generics = value.as_bool(invert)?,
        "disallow_any_decorated" => flags.disallow_any_decorated = value.as_bool(invert)?,
        "disallow_any_explicit" => flags.disallow_any_explicit = value.as_bool(invert)?,
        "disallow_any_unimported" => flags.disallow_any_unimported = value.as_bool(invert)?,
        "disallow_any_expr" => flags.disallow_any_expr = value.as_bool(invert)?,
        "disallow_subclassing_any" => flags.disallow_subclassing_any = value.as_bool(invert)?,
        "disallow_incomplete_defs" => flags.disallow_incomplete_defs = value.as_bool(invert)?,
        "allow_untyped_globals" => flags.allow_untyped_globals = value.as_bool(invert)?,
        "allow_empty_bodies" => flags.allow_empty_bodies = value.as_bool(invert)?,
        "warn_unreachable" => flags.warn_unreachable = value.as_bool(invert)?,
        "warn_return_any" => flags.warn_return_any = value.as_bool(invert)?,
        "warn_no_return" => flags.warn_no_return = value.as_bool(invert)?,
        "local_partial_types" => flags.local_partial_types = value.as_bool(invert)?,
        "implicit_reexport" => flags.no_implicit_reexport = !value.as_bool(invert)?,
        "warn_unused_ignores"
        | "strict_concatenate"
        | "namespace_packages"
        | "explicit_package_bases"
        | "site_packages"
        | "silence_site_packages" => {
            tracing::warn!("TODO ignored config value {name}");
        }

        "extra_checks" => flags.extra_checks = value.as_bool(invert)?,
        // These are currently ignored
        "allow_redefinition" | "follow_imports" | "follow_imports_for_stubs" => (),
        // Will always be irrelevant
        "cache_fine_grained" => (),
        "ignore_errors" => return value.as_bool(invert),
        _ => bail!("Unrecognized option: {original_name} = {}", value.as_repr()),
    }
    Ok(false)
}

fn split_and_trim<'a>(s: &'a str, pattern: &'a [char]) -> impl Iterator<Item = &'a str> {
    let mut s = s.trim();
    if let Some(new_s) = s.strip_suffix(pattern) {
        s = new_s
    }
    s.split(pattern).map(|s| s.trim())
}

fn apply_from_base_config(
    settings: &mut Settings,
    flags: &mut TypeCheckerFlags,
    diagnostic_config: &mut DiagnosticConfig,
    key: &str,
    value: IniOrTomlValue,
) -> ConfigResult {
    match key {
        "show_error_codes" => {
            diagnostic_config.show_error_codes = value.as_bool(false)?;
        }
        "show_column_numbers" => {
            diagnostic_config.show_column_numbers = value.as_bool(false)?;
        }
        "show_error_end" => {
            diagnostic_config.show_error_end = value.as_bool(false)?;
        }
        "python_version"
        | "platform"
        | "show_error_context"
        | "show_traceback"
        | "pretty"
        | "plugins"
        | "enable_incomplete_feature"
        | "show_error_code_links"
        | "cache_dir"
        | "warn_redundant_casts"
        | "warn_unused_configs" => {
            tracing::warn!("TODO ignored config value {key}");
        }
        "files" => {
            settings
                .files_or_directories_to_check
                .extend(value.as_str_list(key, &[','])?);
        }
        "mypy_path" => {
            settings
                .mypy_path
                .extend(value.as_str_list(key, &[',', ':'])?);
        }
        _ => return apply_from_config_part(flags, key, value),
    };
    Ok(false)
}

fn apply_from_config_part(
    flags: &mut TypeCheckerFlags,
    key: &str,
    value: IniOrTomlValue,
) -> ConfigResult {
    if key == "strict" {
        if value.as_bool(false)? {
            flags.enable_all_strict_flags();
        }
        Ok(false)
    } else {
        set_flag_and_return_ignore_errors(flags, key, value)
    }
}

fn add_excludes(excludes: &mut Vec<ExcludeRegex>, value: IniOrTomlValue) -> ConfigResult {
    let mut compile_str = |s| match Regex::new(s) {
        Ok(regex) => {
            excludes.push(ExcludeRegex {
                regex_str: s.into(),
                regex,
            });
            Ok(false)
        }
        Err(err) => bail!(err),
    };
    match &value {
        IniOrTomlValue::Toml(Value::Array(lst)) => {
            for entry in lst.iter() {
                match entry {
                    Value::String(s) => {
                        compile_str(s.value())?;
                    }
                    _ => bail!("TODO expected string array".to_string()),
                }
            }
            Ok(false)
        }
        IniOrTomlValue::Toml(Value::String(s)) => compile_str(s.value()),
        IniOrTomlValue::Ini(v) => compile_str(v),
        _ => bail!("TODO expected string"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_invalid_toml_bool() {
        use super::*;
        let code = "\
            [tool.mypy]\n\
            disallow_any_generics = \"what\"
        ";
        let Err(err) = ProjectOptions::from_pyproject_toml(code, &mut DiagnosticConfig::default())
        else {
            unreachable!()
        };
        assert_eq!(err.to_string(), "Expected bool, got \"what\"");
    }

    #[test]
    fn test_invalid_ini_bool() {
        use super::*;
        let code = "\
            [mypy]\n\
            disallow_any_generics = what
        ";
        let Err(err) = ProjectOptions::from_mypy_ini(code, &mut DiagnosticConfig::default()) else {
            unreachable!()
        };
        assert_eq!(err.to_string(), "Expected bool, got \"what\"");
    }

    #[test]
    fn test_invalid_toml_none() {
        use super::*;
        let code = "[tool.mypy.foo]\nx=1";
        let Err(err) = ProjectOptions::from_pyproject_toml(code, &mut DiagnosticConfig::default())
        else {
            unreachable!()
        };
        assert_eq!(
            err.to_string(),
            "Expected tool.mypy to be simple table in pyproject.toml"
        );
    }
}
