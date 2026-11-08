//! Build-time shader resource budget check for PrismaRev.
//!
//! Folds every descriptor and push-constant block in the committed Slang
//! reflection JSONs into one row per shader module and compares that row with
//! `assets/shaders/budgets.json`. Growing a pass past its row fails the check,
//! so "this pass quietly gained eight sampled textures" becomes a review item
//! instead of a CI-green surprise.
//!
//! Usage:
//!   cargo run --bin shader-budget -- [--reflection-dir DIR] [--budget FILE]
//!                                     [--write-budget] [--json FILE]
//!
//! `--write-budget` rewrites the committed table from the current shaders; use
//! it only when the growth is intended, and review the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const BUDGET_VERSION: u32 = 1;

/// Top-level reflection document. Only the module-wide parameter list matters
/// here: it is the union of every entry point, i.e. the descriptor set layout
/// the pipeline actually binds.
#[derive(Debug, Deserialize)]
struct Reflection {
    #[serde(default)]
    parameters: Vec<Parameter>,
}

#[derive(Debug, Deserialize)]
struct Parameter {
    name: String,
    #[serde(default)]
    binding: Option<Binding>,
    #[serde(rename = "type")]
    ty: Option<TypeInfo>,
}

#[derive(Debug, Deserialize)]
struct Binding {
    kind: String,
}

#[derive(Debug, Deserialize)]
struct TypeInfo {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "baseShape")]
    base_shape: Option<String>,
    #[serde(default, rename = "elementType")]
    element_type: Option<Box<TypeInfo>>,
    #[serde(default)]
    sizes: Vec<SizeEntry>,
    #[serde(default)]
    fields: Vec<FieldInfo>,
}

#[derive(Debug, Deserialize)]
struct SizeEntry {
    kind: String,
    value: u32,
}

#[derive(Debug, Deserialize)]
struct FieldInfo {
    #[serde(default)]
    binding: FieldBinding,
}

#[derive(Debug, Default, Deserialize)]
struct FieldBinding {
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

/// Resource classes that cost bandwidth or descriptor slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ResourceKind {
    SampledImage,
    SampledImageArray,
    Sampler,
    UniformBuffer,
    StorageBuffer,
    AccelerationStructure,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    SampledImages,
    SampledImageArrays,
    Samplers,
    UniformBuffers,
    StorageBuffers,
    AccelerationStructures,
    Descriptors,
    PushConstantBytes,
}

impl Metric {
    /// Serialized key in `budgets.json`; also used in failure messages.
    fn key(self) -> &'static str {
        match self {
            Metric::SampledImages => "sampled_images",
            Metric::SampledImageArrays => "sampled_image_arrays",
            Metric::Samplers => "samplers",
            Metric::UniformBuffers => "uniform_buffers",
            Metric::StorageBuffers => "storage_buffers",
            Metric::AccelerationStructures => "acceleration_structures",
            Metric::Descriptors => "descriptors",
            Metric::PushConstantBytes => "push_constant_bytes",
        }
    }
}

/// One shader's measured usage, and the same fields double as its limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShaderBudget {
    sampled_images: u32,
    sampled_image_arrays: u32,
    samplers: u32,
    uniform_buffers: u32,
    storage_buffers: u32,
    acceleration_structures: u32,
    descriptors: u32,
    push_constant_bytes: u32,
}

impl ShaderBudget {
    fn get(&self, metric: Metric) -> u32 {
        match metric {
            Metric::SampledImages => self.sampled_images,
            Metric::SampledImageArrays => self.sampled_image_arrays,
            Metric::Samplers => self.samplers,
            Metric::UniformBuffers => self.uniform_buffers,
            Metric::StorageBuffers => self.storage_buffers,
            Metric::AccelerationStructures => self.acceleration_structures,
            Metric::Descriptors => self.descriptors,
            Metric::PushConstantBytes => self.push_constant_bytes,
        }
    }

    fn add(&mut self, metric: Metric, amount: u32) {
        let value = self.get(metric) + amount;
        match metric {
            Metric::SampledImages => self.sampled_images = value,
            Metric::SampledImageArrays => self.sampled_image_arrays = value,
            Metric::Samplers => self.samplers = value,
            Metric::UniformBuffers => self.uniform_buffers = value,
            Metric::StorageBuffers => self.storage_buffers = value,
            Metric::AccelerationStructures => self.acceleration_structures = value,
            Metric::Descriptors => self.descriptors = value,
            Metric::PushConstantBytes => self.push_constant_bytes = value,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    version: u32,
    shaders: BTreeMap<String, ShaderBudget>,
}

/// Everything measured for one shader module.
#[derive(Debug, Default)]
struct Measurement {
    budget: ShaderBudget,
    bindings: Vec<(ResourceKind, String)>,
}

impl Measurement {
    fn bindings_for(&self, metric: Metric) -> Vec<&str> {
        self.bindings
            .iter()
            .filter(|(kind, _)| metric_of(*kind) == Some(metric))
            .map(|(_, name)| name.as_str())
            .collect()
    }
}

/// The counter a descriptor class contributes to, if any.
fn metric_of(kind: ResourceKind) -> Option<Metric> {
    match kind {
        ResourceKind::SampledImage => Some(Metric::SampledImages),
        ResourceKind::SampledImageArray => Some(Metric::SampledImageArrays),
        ResourceKind::Sampler => Some(Metric::Samplers),
        ResourceKind::UniformBuffer => Some(Metric::UniformBuffers),
        ResourceKind::StorageBuffer => Some(Metric::StorageBuffers),
        ResourceKind::AccelerationStructure => Some(Metric::AccelerationStructures),
        ResourceKind::Other => None,
    }
}

fn classify_descriptor(parameter: &Parameter) -> ResourceKind {
    let Some(ty) = parameter.ty.as_ref() else {
        return ResourceKind::Other;
    };
    match ty.kind.as_deref().unwrap_or_default() {
        "constantBuffer" => ResourceKind::UniformBuffer,
        "samplerState" => ResourceKind::Sampler,
        "array" => match ty.element_type.as_ref().and_then(|e| e.kind.as_deref()) {
            Some("samplerState") => ResourceKind::Sampler,
            Some("resource") => ResourceKind::SampledImageArray,
            _ => ResourceKind::Other,
        },
        "resource" => {
            let shape = ty.base_shape.as_deref().unwrap_or_default();
            if shape.starts_with("texture") {
                ResourceKind::SampledImage
            } else if shape.contains("Buffer") {
                ResourceKind::StorageBuffer
            } else if shape == "accelerationStructure" {
                ResourceKind::AccelerationStructure
            } else {
                ResourceKind::Other
            }
        }
        _ => ResourceKind::Other,
    }
}

/// Push-constant bytes from the reflected struct. `sizes` gives the whole
/// block; older reflections only carry per-field offsets, so fall back to the
/// end of the last field.
fn push_constant_size(parameter: &Parameter) -> u32 {
    let Some(element) = parameter
        .ty
        .as_ref()
        .and_then(|ty| ty.element_type.as_deref())
    else {
        return 0;
    };
    if let Some(size) = element.sizes.iter().find(|entry| entry.kind == "uniform") {
        return size.value;
    }
    element
        .fields
        .iter()
        .filter_map(|field| Some(field.binding.offset? + field.binding.size?))
        .max()
        .unwrap_or(0)
}

fn measure_shader(reflection: &Reflection) -> Measurement {
    let mut measurement = Measurement::default();
    for parameter in &reflection.parameters {
        let Some(binding) = parameter.binding.as_ref() else {
            continue;
        };
        if binding.kind.starts_with("pushConstant") {
            measurement
                .budget
                .add(Metric::PushConstantBytes, push_constant_size(parameter));
            continue;
        }
        if binding.kind != "descriptorTableSlot" && binding.kind != "uniform" {
            continue;
        }
        measurement
            .bindings
            .push((classify_descriptor(parameter), parameter.name.clone()));
    }

    measurement
        .budget
        .add(Metric::Descriptors, measurement.bindings.len() as u32);
    let bindings = measurement.bindings.clone();
    for (kind, _) in &bindings {
        if let Some(metric) = metric_of(*kind) {
            measurement.budget.add(metric, 1);
        }
    }
    measurement
}

#[derive(Debug)]
enum Problem {
    /// The shader exists but the budget table has no row for it.
    MissingRow { descriptors: u32 },
    /// The budget table has a row but the shader is gone.
    StaleRow,
    Exceeded {
        metric: Metric,
        actual: u32,
        limit: u32,
        bindings: Vec<String>,
    },
}

#[derive(Debug)]
struct Violation {
    shader: String,
    problem: Problem,
}

impl Violation {
    fn describe(&self) -> String {
        match &self.problem {
            Problem::MissingRow { descriptors } => format!(
                "{}: no budget row (measured {} descriptor(s)); run shader-budget --write-budget",
                self.shader, descriptors
            ),
            Problem::StaleRow => format!(
                "{}: budget row has no matching reflection JSON; drop the row",
                self.shader
            ),
            Problem::Exceeded {
                metric,
                actual,
                limit,
                bindings,
            } => {
                let mut line = format!("{}: {} {} > {}", self.shader, metric.key(), actual, limit);
                if !bindings.is_empty() {
                    line.push_str(&format!(" (bindings: {})", bindings.join(", ")));
                }
                line
            }
        }
    }
}

fn compare(measured: &BTreeMap<String, Measurement>, limits: &BudgetFile) -> Vec<Violation> {
    const METRICS: [Metric; 8] = [
        Metric::SampledImages,
        Metric::SampledImageArrays,
        Metric::Samplers,
        Metric::UniformBuffers,
        Metric::StorageBuffers,
        Metric::AccelerationStructures,
        Metric::Descriptors,
        Metric::PushConstantBytes,
    ];

    let mut violations = Vec::new();
    for (shader, measurement) in measured {
        let Some(limit) = limits.shaders.get(shader) else {
            violations.push(Violation {
                shader: shader.clone(),
                problem: Problem::MissingRow {
                    descriptors: measurement.budget.descriptors,
                },
            });
            continue;
        };
        for metric in METRICS {
            let actual = measurement.budget.get(metric);
            if actual > limit.get(metric) {
                violations.push(Violation {
                    shader: shader.clone(),
                    problem: Problem::Exceeded {
                        metric,
                        actual,
                        limit: limit.get(metric),
                        bindings: measurement
                            .bindings_for(metric)
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                    },
                });
            }
        }
    }
    for shader in limits.shaders.keys() {
        if !measured.contains_key(shader) {
            violations.push(Violation {
                shader: shader.clone(),
                problem: Problem::StaleRow,
            });
        }
    }
    violations
}

fn measure_dir(dir: &Path) -> Result<BTreeMap<String, Measurement>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().map(|ext| ext == "json").unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!(
            "no reflection JSON found in {} — run assets/shaders/compile.sh first",
            dir.display()
        );
    }

    let mut measured = BTreeMap::new();
    for path in files {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let reflection: Reflection = serde_json::from_str(&raw)
            .with_context(|| format!("parse reflection JSON {}", path.display()))?;
        let shader = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("shader")
            .to_string();
        measured.insert(shader, measure_shader(&reflection));
    }
    Ok(measured)
}

fn read_budget(path: &Path) -> Result<BudgetFile> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "read {} — create it with `cargo run --bin shader-budget -- --write-budget`",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn write_budget(path: &Path, file: &BudgetFile) -> Result<()> {
    let mut text = serde_json::to_string_pretty(file)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

#[derive(Debug)]
struct Options {
    reflection_dir: PathBuf,
    budget: PathBuf,
    write_budget: bool,
    json: Option<PathBuf>,
}

fn default_reflection_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/reflection")
}

fn default_budget_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/budgets.json")
}

const USAGE: &str = "\
usage: shader-budget [--reflection-dir DIR] [--budget FILE] [--write-budget] [--json FILE]

  --reflection-dir DIR  Slang reflection JSON directory (default: assets/shaders/reflection)
  --budget FILE         Budget table (default: assets/shaders/budgets.json)
  --write-budget        Rewrite the budget table from the current shaders
  --json FILE           Also write the measured usage vs limits as JSON";

fn parse_options() -> Result<Option<Options>> {
    let mut options = Options {
        reflection_dir: default_reflection_dir(),
        budget: default_budget_path(),
        write_budget: false,
        json: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--reflection-dir" => {
                options.reflection_dir =
                    PathBuf::from(args.next().context("--reflection-dir needs a value")?);
            }
            "--budget" => {
                options.budget = PathBuf::from(args.next().context("--budget needs a value")?);
            }
            "--write-budget" => options.write_budget = true,
            "--json" => {
                options.json = Some(PathBuf::from(args.next().context("--json needs a value")?));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            other => bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }
    Ok(Some(options))
}

#[derive(Serialize)]
struct Report<'a> {
    version: u32,
    measured: &'a BTreeMap<String, ShaderBudget>,
    limits: &'a BTreeMap<String, ShaderBudget>,
}

fn main() -> Result<()> {
    let Some(options) = parse_options()? else {
        return Ok(());
    };

    let measured = measure_dir(&options.reflection_dir)?;
    if options.write_budget {
        let file = BudgetFile {
            version: BUDGET_VERSION,
            shaders: measured
                .iter()
                .map(|(shader, measurement)| (shader.clone(), measurement.budget))
                .collect(),
        };
        write_budget(&options.budget, &file)?;
        println!(
            "wrote {} shader budget row(s) to {}",
            file.shaders.len(),
            options.budget.display()
        );
        return Ok(());
    }

    let limits = read_budget(&options.budget)?;
    if let Some(path) = options.json.as_ref() {
        let measured_rows: BTreeMap<String, ShaderBudget> = measured
            .iter()
            .map(|(shader, measurement)| (shader.clone(), measurement.budget))
            .collect();
        let report = Report {
            version: BUDGET_VERSION,
            measured: &measured_rows,
            limits: &limits.shaders,
        };
        let mut text = serde_json::to_string_pretty(&report)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    }

    let violations = compare(&measured, &limits);
    if violations.is_empty() {
        println!("shader budget: {} shader(s) within limits", measured.len());
        return Ok(());
    }

    eprintln!("shader budget exceeded:");
    for violation in &violations {
        eprintln!("  {}", violation.describe());
    }
    eprintln!(
        "  Update {} only when the growth is intended; review the diff.",
        options.budget.display()
    );
    bail!("{} shader budget violation(s)", violations.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed_dir() -> PathBuf {
        default_reflection_dir()
    }

    fn parse(text: &str) -> Reflection {
        serde_json::from_str(text).expect("parse synthetic reflection")
    }

    #[test]
    fn committed_shaders_fit_their_budgets() {
        let measured = measure_dir(&committed_dir()).expect("measure committed reflections");
        let limits = read_budget(&default_budget_path()).expect("read budgets.json");
        let violations = compare(&measured, &limits);
        assert!(
            violations.is_empty(),
            "budget violations: {:?}",
            violations
                .iter()
                .map(Violation::describe)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn classifies_every_descriptor_shape() {
        let reflection = parse(
            r#"{
                "parameters": [
                    {"name": "frame", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "constantBuffer"}},
                    {"name": "albedo", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "texture2D"}},
                    {"name": "env", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "textureCube"}},
                    {"name": "materials", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "structuredBuffer"}},
                    {"name": "shadowSampler", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "samplerState"}},
                    {"name": "bindlessSrvs", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "array", "elementType": {"kind": "resource"}}},
                    {"name": "globalSamplers", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "array", "elementType": {"kind": "samplerState"}}},
                    {"name": "tlas", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "accelerationStructure"}},
                    {"name": "vertexBuffer", "binding": {"kind": "vertexInput"},
                     "type": {"kind": "vector"}}
                ],
                "entryPoints": []
            }"#,
        );

        let measurement = measure_shader(&reflection);
        assert_eq!(measurement.bindings.len(), 8);
        assert_eq!(measurement.budget.sampled_images, 2);
        assert_eq!(measurement.budget.sampled_image_arrays, 1);
        assert_eq!(measurement.budget.samplers, 2);
        assert_eq!(measurement.budget.uniform_buffers, 1);
        assert_eq!(measurement.budget.storage_buffers, 1);
        assert_eq!(measurement.budget.acceleration_structures, 1);
        assert_eq!(measurement.budget.descriptors, 8);
    }

    #[test]
    fn reads_push_constant_size_from_the_block() {
        let reflection = parse(
            r#"{
                "parameters": [
                    {"name": "pc", "binding": {"kind": "pushConstantBuffer"},
                     "type": {"kind": "constantBuffer", "elementType": {
                        "kind": "struct",
                        "sizes": [{"kind": "uniform", "value": 96}],
                        "fields": [{"binding": {"offset": 0, "size": 64}}]
                     }}}
                ]
            }"#,
        );

        let measurement = measure_shader(&reflection);
        assert_eq!(measurement.budget.push_constant_bytes, 96);
        assert_eq!(measurement.budget.descriptors, 0);
    }

    #[test]
    fn falls_back_to_the_last_field_end() {
        let reflection = parse(
            r#"{
                "parameters": [
                    {"name": "pc", "binding": {"kind": "pushConstant"},
                     "type": {"kind": "constantBuffer", "elementType": {
                        "fields": [
                            {"binding": {"offset": 0, "size": 64}},
                            {"binding": {"offset": 68, "size": 4}}
                        ]
                     }}}
                ]
            }"#,
        );

        assert_eq!(measure_shader(&reflection).budget.push_constant_bytes, 72);
    }

    #[test]
    fn reports_the_metric_and_its_bindings() {
        let reflection = parse(
            r#"{
                "parameters": [
                    {"name": "albedo", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "texture2D"}},
                    {"name": "normal", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "resource", "baseShape": "texture2D"}},
                    {"name": "frame", "binding": {"kind": "descriptorTableSlot"},
                     "type": {"kind": "constantBuffer"}}
                ]
            }"#,
        );
        let mut measured = BTreeMap::new();
        measured.insert("scene_frag".to_string(), measure_shader(&reflection));

        let mut limits = BTreeMap::new();
        limits.insert(
            "scene_frag".to_string(),
            ShaderBudget {
                sampled_images: 1,
                uniform_buffers: 1,
                descriptors: 3,
                ..ShaderBudget::default()
            },
        );
        let budget_file = BudgetFile {
            version: BUDGET_VERSION,
            shaders: limits,
        };

        let violations = compare(&measured, &budget_file);
        assert_eq!(violations.len(), 1);
        let text = violations[0].describe();
        assert!(text.contains("sampled_images 2 > 1"), "{text}");
        assert!(text.contains("albedo, normal"), "{text}");
    }

    #[test]
    fn flags_a_shader_without_a_budget_row() {
        let measured = measure_dir(&committed_dir()).expect("measure committed reflections");
        let budget_file = BudgetFile {
            version: BUDGET_VERSION,
            shaders: BTreeMap::new(),
        };

        let violations = compare(&measured, &budget_file);
        assert_eq!(violations.len(), measured.len());
        assert!(violations
            .iter()
            .all(|violation| violation.describe().contains("no budget row")));
    }
}
