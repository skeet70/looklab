// SPDX-License-Identifier: AGPL-3.0-or-later
//! The proposal form ([docs/08 §4](../../../docs/08-ui.md)).

use std::str::FromStr;

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_publish::{BodyPage, SiteIndex};
use looklab_schema::{
    BodyId, Control, ControlChange, ControlDefault, ControlDefinition, ControlEdit, ControlKey,
    ControlSchema, ControlType, LutBase, LutDynamicRange, LutRenderer, MakerName, MappingStatus,
    ProposalKind, ProposalView, Range, RenderingName, ScalarValue, SimKey, ValidatedPayload,
    validate_proposal,
};

use crate::proposal_form::{self, BaseLookVersion, FixtureFile, SimIdentity};
use crate::{data, files};

const EXAMPLE_FILES: &str = "\
classic_chrome FilmMode=0x0600
provia FilmMode=0x0000
acros FilmMode=0x0500 GrainSize=0x0002";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MappingMode {
    New,
    Existing,
}

impl std::fmt::Display for MappingMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::New => "new",
            Self::Existing => "existing",
        })
    }
}

impl FromStr for MappingMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "new" => Ok(Self::New),
            "existing" => Ok(Self::Existing),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyKindChoice {
    Camera,
    Software,
}

impl BodyKindChoice {
    fn body_kind(self) -> looklab_schema::BodyKind {
        match self {
            Self::Camera => looklab_schema::BodyKind::Camera,
            Self::Software => looklab_schema::BodyKind::Software,
        }
    }
}

impl std::fmt::Display for BodyKindChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Camera => "camera",
            Self::Software => "software",
        })
    }
}

impl FromStr for BodyKindChoice {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "camera" => Ok(Self::Camera),
            "software" => Ok(Self::Software),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MappingStatusChoice {
    Provisional,
    FixtureBacked,
}

impl MappingStatusChoice {
    fn mapping_status(self) -> MappingStatus {
        match self {
            Self::Provisional => MappingStatus::Provisional,
            Self::FixtureBacked => MappingStatus::FixtureBacked,
        }
    }
}

impl std::fmt::Display for MappingStatusChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Provisional => "provisional",
            Self::FixtureBacked => "fixture_backed",
        })
    }
}

impl FromStr for MappingStatusChoice {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "provisional" => Ok(Self::Provisional),
            "fixture_backed" => Ok(Self::FixtureBacked),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlTypeChoice {
    Int,
    Decimal,
    Enum,
    Bool,
}

impl ControlTypeChoice {
    fn control_type(self) -> ControlType {
        match self {
            Self::Int => ControlType::Int,
            Self::Decimal => ControlType::Decimal,
            Self::Enum => ControlType::Enum,
            Self::Bool => ControlType::Bool,
        }
    }
}

impl std::fmt::Display for ControlTypeChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Enum => "enum",
            Self::Bool => "bool",
        })
    }
}

impl FromStr for ControlTypeChoice {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "int" => Ok(Self::Int),
            "decimal" => Ok(Self::Decimal),
            "enum" => Ok(Self::Enum),
            "bool" => Ok(Self::Bool),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LutEncodingMode {
    Scene,
    Display,
}

impl std::fmt::Display for LutEncodingMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Scene => "scene",
            Self::Display => "display",
        })
    }
}

impl FromStr for LutEncodingMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "scene" => Ok(Self::Scene),
            "display" => Ok(Self::Display),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LutBaseMode {
    Look,
    BodyRealization,
    FamilyRendering,
}

impl std::fmt::Display for LutBaseMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Look => "look",
            Self::BodyRealization => "body_realization",
            Self::FamilyRendering => "family_rendering",
        })
    }
}

impl FromStr for LutBaseMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "look" => Ok(Self::Look),
            "body_realization" => Ok(Self::BodyRealization),
            "family_rendering" => Ok(Self::FamilyRendering),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyPickerMode {
    Catalog,
    Unregistered,
}

impl std::fmt::Display for BodyPickerMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Catalog => "catalog",
            Self::Unregistered => "unregistered",
        })
    }
}

impl FromStr for BodyPickerMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "catalog" => Ok(Self::Catalog),
            "unregistered" => Ok(Self::Unregistered),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LutEmulationMode {
    Original,
    CameraRendering,
    FilmStock,
}

impl std::fmt::Display for LutEmulationMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Original => "original",
            Self::CameraRendering => "camera",
            Self::FilmStock => "film",
        })
    }
}

impl FromStr for LutEmulationMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "original" => Ok(Self::Original),
            "camera" => Ok(Self::CameraRendering),
            "film" => Ok(Self::FilmStock),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MenuFileDraft {
    name: String,
    answer: String,
    parsed_values: String,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingDonations {
    files: Vec<FixtureFile>,
    make: String,
    model: String,
    kind: ProposalKind,
    payload: String,
    lut_file: Option<proposal_form::LutFileDraft>,
}

#[derive(Clone, Debug, PartialEq)]
struct SchemaEditRow {
    epoch: u32,
    label: String,
    change: ControlChange,
}

#[derive(Clone, Debug, PartialEq)]
struct ControlDraft {
    min: String,
    max: String,
    step: String,
    default: String,
    values: String,
    look: bool,
    affects: String,
    spatial: bool,
    tolerated: String,
}

impl ControlDraft {
    fn from_control(control: &Control) -> Self {
        Self {
            min: control
                .range
                .as_ref()
                .map(|range| range.min.to_string())
                .unwrap_or_default(),
            max: control
                .range
                .as_ref()
                .map(|range| range.max.to_string())
                .unwrap_or_default(),
            step: control
                .step
                .map(|step| step.to_string())
                .unwrap_or_default(),
            default: proposal_form::scalar_text(&default_scalar(&control.default)),
            values: control
                .values
                .as_ref()
                .map(|values| {
                    values
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            look: control.look_affecting,
            affects: control
                .affects
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            spatial: control.spatial,
            tolerated: control
                .tolerated
                .as_ref()
                .map(proposal_form::tolerated_text)
                .unwrap_or_default(),
        }
    }
}

fn default_scalar(default: &ControlDefault) -> ScalarValue {
    match default {
        ControlDefault::Number(value) => ScalarValue::Number(*value),
        ControlDefault::Enum(value) => ScalarValue::Text(value.to_string()),
        ControlDefault::Bool(value) => ScalarValue::Bool(*value),
    }
}

/// Identifies one pending first-schema file read; a newer text edit supersedes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FirstSchemaRead(u64);

impl FirstSchemaRead {
    fn supersede(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// Whether a completed read may fill the textarea.
    fn is_current(self, read: Self) -> bool {
        self == read
    }
}

#[derive(Clone, Copy)]
struct Form {
    kind: RwSignal<ProposalKind>,
    manual: RwSignal<Option<String>>,
    index: RwSignal<Option<SiteIndex>>,
    index_error: RwSignal<Option<String>>,
    body_make: RwSignal<String>,
    body_model: RwSignal<String>,
    body_kind: RwSignal<looklab_schema::BodyKind>,
    body_mapping: RwSignal<MappingStatus>,
    body_raw: RwSignal<bool>,
    body_heif: RwSignal<bool>,
    body_spaces: RwSignal<String>,
    body_aliases: RwSignal<String>,
    mapping_camera: RwSignal<Option<BodyId>>,
    mapping_schema: RwSignal<Option<ControlSchema>>,
    mapping_error: RwSignal<Option<String>>,
    mapping_request: RwSignal<u64>,
    mapping_mode: RwSignal<MappingMode>,
    mapping_name: RwSignal<String>,
    mapping_existing: RwSignal<Option<SimIdentity>>,
    mapping_epoch: RwSignal<u32>,
    mapping_tag: RwSignal<String>,
    mapping_value: RwSignal<String>,
    mapping_monochrome: RwSignal<bool>,
    mapping_files_text: RwSignal<String>,
    mapping_labeled: RwSignal<Vec<MenuFileDraft>>,
    schema_camera: RwSignal<Option<BodyId>>,
    schema_page: RwSignal<Option<BodyPage>>,
    schema_error: RwSignal<Option<String>>,
    schema_request: RwSignal<u64>,
    schema_epoch: RwSignal<u32>,
    schema_selected: RwSignal<Vec<ControlKey>>,
    schema_edits: RwSignal<Vec<SchemaEditRow>>,
    first_schema_camera: RwSignal<Option<BodyId>>,
    first_schema_text: RwSignal<String>,
    first_schema_error: RwSignal<Option<String>>,
    first_schema_generation: RwSignal<FirstSchemaRead>,
    body_context: RwSignal<Option<Vec<looklab_schema::Body>>>,
    body_context_error: RwSignal<Option<String>>,
    add_key: RwSignal<String>,
    add_type: RwSignal<ControlType>,
    add_min: RwSignal<String>,
    add_max: RwSignal<String>,
    add_step: RwSignal<String>,
    add_default: RwSignal<String>,
    add_values: RwSignal<String>,
    add_look: RwSignal<bool>,
    add_affects: RwSignal<String>,
    add_spatial: RwSignal<bool>,
    add_tolerated: RwSignal<String>,
    lut_file: RwSignal<Option<proposal_form::LutFileDraft>>,
    lut_name: RwSignal<String>,
    lut_attribution: RwSignal<String>,
    lut_own_work: RwSignal<bool>,
    lut_source_license: RwSignal<String>,
    lut_source_url: RwSignal<String>,
    lut_encoding: RwSignal<LutEncodingMode>,
    lut_base_mode: RwSignal<LutBaseMode>,
    lut_base: RwSignal<Option<BaseLookVersion>>,
    lut_base_body: RwSignal<Option<BodyId>>,
    lut_base_body_mode: RwSignal<BodyPickerMode>,
    lut_base_body_text: RwSignal<String>,
    lut_base_sim: RwSignal<String>,
    lut_base_dynamic_range: RwSignal<Option<LutDynamicRange>>,
    lut_base_notes: RwSignal<String>,
    lut_family_maker: RwSignal<String>,
    lut_family_rendering: RwSignal<String>,
    lut_family_dynamic_range: RwSignal<Option<LutDynamicRange>>,
    lut_family_notes: RwSignal<String>,
    lut_emulation: RwSignal<LutEmulationMode>,
    lut_emulation_maker: RwSignal<String>,
    lut_emulation_rendering: RwSignal<String>,
    lut_emulation_stock: RwSignal<String>,
    lut_generation: RwSignal<u64>,
    interface_text: RwSignal<String>,
    interface_error: RwSignal<Option<String>>,
    interface_summary: RwSignal<Option<(String, String)>>,
    interface_generation: RwSignal<u64>,
    fixture_files: RwSignal<Vec<FixtureFile>>,
    consented: RwSignal<Vec<bool>>,
    fixture_generation: RwSignal<u64>,
    busy: RwSignal<bool>,
    message: RwSignal<Option<String>>,
    submitted: RwSignal<Option<ProposalView>>,
    pending: RwSignal<Option<PendingDonations>>,
}

impl Form {
    fn new(initial: ProposalKind) -> Self {
        Self {
            kind: RwSignal::new(initial),
            manual: RwSignal::new(None),
            index: RwSignal::new(None),
            index_error: RwSignal::new(None),
            body_make: RwSignal::new(String::new()),
            body_model: RwSignal::new(String::new()),
            body_kind: RwSignal::new(looklab_schema::BodyKind::Camera),
            body_mapping: RwSignal::new(MappingStatus::Provisional),
            body_raw: RwSignal::new(true),
            body_heif: RwSignal::new(false),
            body_spaces: RwSignal::new("srgb".to_owned()),
            body_aliases: RwSignal::new(String::new()),
            mapping_camera: RwSignal::new(None),
            mapping_schema: RwSignal::new(None),
            mapping_error: RwSignal::new(None),
            mapping_request: RwSignal::new(0),
            mapping_mode: RwSignal::new(MappingMode::New),
            mapping_name: RwSignal::new(String::new()),
            mapping_existing: RwSignal::new(None),
            mapping_epoch: RwSignal::new(1),
            mapping_tag: RwSignal::new("FilmMode".to_owned()),
            mapping_value: RwSignal::new(String::new()),
            mapping_monochrome: RwSignal::new(false),
            mapping_files_text: RwSignal::new(EXAMPLE_FILES.to_owned()),
            mapping_labeled: RwSignal::new(Vec::new()),
            schema_camera: RwSignal::new(None),
            schema_page: RwSignal::new(None),
            schema_error: RwSignal::new(None),
            schema_request: RwSignal::new(0),
            schema_epoch: RwSignal::new(1),
            schema_selected: RwSignal::new(Vec::new()),
            schema_edits: RwSignal::new(Vec::new()),
            first_schema_camera: RwSignal::new(None),
            first_schema_text: RwSignal::new(String::new()),
            first_schema_error: RwSignal::new(None),
            first_schema_generation: RwSignal::new(FirstSchemaRead::default()),
            body_context: RwSignal::new(None),
            body_context_error: RwSignal::new(None),
            add_key: RwSignal::new(String::new()),
            add_type: RwSignal::new(ControlType::Int),
            add_min: RwSignal::new(String::new()),
            add_max: RwSignal::new(String::new()),
            add_step: RwSignal::new(String::new()),
            add_default: RwSignal::new(String::new()),
            add_values: RwSignal::new(String::new()),
            add_look: RwSignal::new(false),
            add_affects: RwSignal::new(String::new()),
            add_spatial: RwSignal::new(false),
            add_tolerated: RwSignal::new(String::new()),
            lut_file: RwSignal::new(None),
            lut_name: RwSignal::new(String::new()),
            lut_attribution: RwSignal::new(String::new()),
            lut_own_work: RwSignal::new(false),
            lut_source_license: RwSignal::new(String::new()),
            lut_source_url: RwSignal::new(String::new()),
            lut_encoding: RwSignal::new(LutEncodingMode::Scene),
            lut_base_mode: RwSignal::new(LutBaseMode::Look),
            lut_base: RwSignal::new(None),
            lut_base_body: RwSignal::new(None),
            lut_base_body_mode: RwSignal::new(BodyPickerMode::Catalog),
            lut_base_body_text: RwSignal::new(String::new()),
            lut_base_sim: RwSignal::new(String::new()),
            lut_base_dynamic_range: RwSignal::new(None),
            lut_base_notes: RwSignal::new(String::new()),
            lut_family_maker: RwSignal::new(String::new()),
            lut_family_rendering: RwSignal::new(String::new()),
            lut_family_dynamic_range: RwSignal::new(None),
            lut_family_notes: RwSignal::new(String::new()),
            lut_emulation: RwSignal::new(LutEmulationMode::Original),
            lut_emulation_maker: RwSignal::new(String::new()),
            lut_emulation_rendering: RwSignal::new(String::new()),
            lut_emulation_stock: RwSignal::new(String::new()),
            lut_generation: RwSignal::new(0),
            interface_text: RwSignal::new(String::new()),
            interface_error: RwSignal::new(None),
            interface_summary: RwSignal::new(None),
            interface_generation: RwSignal::new(0),
            fixture_files: RwSignal::new(Vec::new()),
            consented: RwSignal::new(Vec::new()),
            fixture_generation: RwSignal::new(0),
            busy: RwSignal::new(false),
            message: RwSignal::new(None),
            submitted: RwSignal::new(None),
            pending: RwSignal::new(None),
        }
    }

    fn reset_evidence(&self) {
        self.pending.set(None);
        self.fixture_generation
            .update(|generation| *generation = generation.wrapping_add(1));
        self.fixture_files.set(Vec::new());
        self.consented.set(Vec::new());
    }

    fn first_schema_read_is_current(&self, read: FirstSchemaRead) -> bool {
        self.first_schema_generation
            .try_get_untracked()
            .is_some_and(|current| current.is_current(read))
    }

    fn reset_for_kind(&self, kind: ProposalKind) {
        self.kind.set(kind);
        self.manual.set(None);
        self.message.set(None);
        self.submitted.set(None);
        self.first_schema_camera.set(None);
        self.first_schema_text.set(String::new());
        self.first_schema_error.set(None);
        self.first_schema_generation
            .update(|read| *read = read.supersede());
        self.reset_evidence();
    }
}

fn computed_payload(form: &Form) -> Result<String, String> {
    match form.kind.get() {
        ProposalKind::Body => {
            let make = form.body_make.get();
            let model = form.body_model.get();
            if make.trim().is_empty() {
                return Err("Enter the camera make, for example Fujifilm.".to_owned());
            }
            if model.trim().is_empty() {
                return Err("Enter the camera model, for example X-T4.".to_owned());
            }
            proposal_form::BodyDraft {
                make,
                model,
                kind: form.body_kind.get(),
                mapping_status: form.body_mapping.get(),
                in_camera_raw_conversion: form.body_raw.get(),
                jpeg_color_spaces: split_list(&form.body_spaces.get()),
                heif: form.body_heif.get(),
                exif_aliases: form
                    .body_aliases
                    .get()
                    .lines()
                    .map(str::trim)
                    .filter(|alias| !alias.is_empty())
                    .map(str::to_owned)
                    .collect(),
            }
            .payload()
        }
        ProposalKind::Mapping => mapping_payload(form),
        ProposalKind::SchemaChange => schema_payload(form),
        ProposalKind::LutLook => lut_payload(form),
        ProposalKind::InterfaceProfile => {
            let text = form.interface_text.get();
            if text.trim().is_empty() {
                return Err("Choose an editing-app profile file.".to_owned());
            }
            looklab_schema::validate_interface_profile(&text)
                .map_err(|errors| join_errors(&errors))?;
            Ok(text)
        }
        ProposalKind::EpochSplit | ProposalKind::LookSplit | ProposalKind::GateOverride => {
            Err("This proposal kind comes from a worker or a moderator.".to_owned())
        }
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split([',', ' '])
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn mapping_payload(form: &Form) -> Result<String, String> {
    let schema = form
        .mapping_schema
        .get()
        .ok_or_else(|| "Choose a camera.".to_owned())?;
    let epoch = schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == form.mapping_epoch.get())
        .ok_or_else(|| "Choose a firmware range.".to_owned())?;
    let (sim_key, look, tag, value, monochrome) = match form.mapping_mode.get() {
        MappingMode::New => {
            let name = form.mapping_name.get();
            if name.trim().is_empty() {
                return Err("Name the film simulation, for example Classic Chrome.".to_owned());
            }
            let sim_key =
                looklab_schema::sim_key_from_name(&name).map_err(|error| error.to_string())?;
            let look =
                looklab_schema::look_id_from_name(&name).map_err(|error| error.to_string())?;
            let tag = looklab_schema::MakernoteTag::try_from(form.mapping_tag.get().trim())
                .map_err(|error| error.to_string())?;
            let value = parse_makernote_value(&form.mapping_value.get())?;
            (sim_key, look, tag, value, form.mapping_monochrome.get())
        }
        MappingMode::Existing => {
            let identity = form
                .mapping_existing
                .get()
                .ok_or_else(|| "Choose the simulation to correct.".to_owned())?;
            let sim = proposal_form::sim_correction(&schema, &identity, epoch.id)?;
            (
                sim.key,
                sim.look,
                sim.makernote.tag,
                sim.makernote.value.0,
                sim.monochrome,
            )
        }
    };
    proposal_form::MappingDraft {
        body: schema.body.clone(),
        epoch: epoch.id,
        sim_key,
        look,
        firmware_from: epoch.firmware.from.clone(),
        tag,
        value,
        monochrome,
    }
    .payload()
}

fn schema_payload(form: &Form) -> Result<String, String> {
    if let Some(camera) = form.first_schema_camera.get() {
        let text = form.first_schema_text.get();
        if text.trim().is_empty() {
            return Err("Paste or load the complete control schema.".to_owned());
        }
        let schema = looklab_schema::validate_control_schema(&text)
            .map_err(|errors| join_errors(&errors))?;
        if schema.body != camera {
            return Err(format!(
                "The schema names {}, not the selected camera.",
                schema.body
            ));
        }
        if !form
            .body_context
            .get()
            .unwrap_or_default()
            .iter()
            .any(|body| body.id == camera)
        {
            return Err("Choose a registered camera without a published schema.".to_owned());
        }
        return Ok(text);
    }
    let page = form
        .schema_page
        .get()
        .ok_or_else(|| "Choose a camera.".to_owned())?;
    let edits = form.schema_edits.get();
    if edits.is_empty() {
        return Err("Add at least one control change.".to_owned());
    }
    let changes = edits
        .into_iter()
        .map(|row| (row.epoch, row.change))
        .collect::<Vec<_>>();
    proposal_form::schema_payload(&page.schema, &changes).map_err(|errors| join_errors(&errors))
}

fn lut_payload(form: &Form) -> Result<String, String> {
    let file = form
        .lut_file
        .get()
        .ok_or_else(|| "Choose a .cube file.".to_owned())?;
    let name = form.lut_name.get();
    if name.trim().is_empty() {
        return Err("Name the look.".to_owned());
    }
    let look = looklab_schema::look_id_from_name(&name).map_err(|error| error.to_string())?;
    looklab_schema::validate_slug(look.as_str()).map_err(|error| error.to_string())?;
    if look.as_str() == looklab_schema::RESERVED_LUT_LOOK_SLUG {
        return Err(
            "That name reserves the fixed neutral baseline link. Choose another name.".to_owned(),
        );
    }
    let encoding = match form.lut_encoding.get() {
        LutEncodingMode::Scene => looklab_schema::LutEncoding::Flog2Fgamut,
        LutEncodingMode::Display => looklab_schema::LutEncoding::Display {
            base: lut_base(form)?,
        },
    };
    let license = looklab_schema::LicenseDeclaration {
        own_work: form.lut_own_work.get(),
        source_license: (!form.lut_source_license.get().trim().is_empty())
            .then(|| form.lut_source_license.get()),
        source_url: (!form.lut_source_url.get().trim().is_empty())
            .then(|| form.lut_source_url.get()),
    };
    let emulates = match form.lut_emulation.get() {
        LutEmulationMode::Original => None,
        LutEmulationMode::CameraRendering => {
            Some(looklab_schema::LutEmulationDecl::CameraRendering {
                maker: form.lut_emulation_maker.get(),
                rendering: form.lut_emulation_rendering.get(),
            })
        }
        LutEmulationMode::FilmStock => Some(looklab_schema::LutEmulationDecl::FilmStock {
            name: form.lut_emulation_stock.get(),
        }),
    };
    proposal_form::LutLookDraft {
        look,
        name,
        license,
        attribution: form.lut_attribution.get(),
        lut: file.file(),
        encoding,
        emulates,
    }
    .payload()
}

fn lut_base(form: &Form) -> Result<LutBase, String> {
    match form.lut_base_mode.get() {
        LutBaseMode::Look => {
            let base = form.lut_base.get().ok_or_else(|| {
                "Choose a published base Look version, or the neutral baseline.".to_owned()
            })?;
            Ok(LutBase::Look {
                look: base.look,
                version: base.version,
            })
        }
        LutBaseMode::BodyRealization => {
            let body = form.lut_base_body.get().ok_or_else(|| {
                if form.lut_base_body_mode.get() == BodyPickerMode::Unregistered
                    && !form.lut_base_body_text.get().trim().is_empty()
                {
                    "Enter the body id as make/model.".to_owned()
                } else {
                    "Choose or enter a base camera body.".to_owned()
                }
            })?;
            let sim = form.lut_base_sim.get();
            let sim = sim.trim();
            let film_simulation = (!sim.is_empty())
                .then(|| SimKey::new(sim))
                .transpose()
                .map_err(|error| error.to_string())?;
            Ok(LutBase::BodyRealization {
                body,
                renderer: LutRenderer::XRawStudio,
                film_simulation,
                dynamic_range: form.lut_base_dynamic_range.get(),
                notes: form.lut_base_notes.get().trim().to_owned(),
            })
        }
        LutBaseMode::FamilyRendering => {
            let maker = form.lut_family_maker.get();
            let rendering = form.lut_family_rendering.get();
            if maker.trim().is_empty() || rendering.trim().is_empty() {
                return Err("Name the camera maker and rendering for the family base.".to_owned());
            }
            Ok(LutBase::FamilyRendering {
                maker: MakerName::new(maker).map_err(|error| error.to_string())?,
                rendering: RenderingName::new(rendering).map_err(|error| error.to_string())?,
                dynamic_range: form.lut_family_dynamic_range.get(),
                notes: form.lut_family_notes.get().trim().to_owned(),
            })
        }
    }
}

fn parse_makernote_value(text: &str) -> Result<u64, String> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    }
    .map_err(|_| "Enter the makernote value as a number or 0x hex.".to_owned())
}

fn join_errors(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn camera_summary(index: &SiteIndex, id: &BodyId) -> Option<(String, String)> {
    index
        .bodies
        .iter()
        .find(|camera| &camera.id == id)
        .map(|camera| (camera.make.clone(), camera.model.clone()))
}

fn form_evidence_identity(form: &Form) -> (String, String) {
    match form.kind.get() {
        ProposalKind::Mapping => form
            .index
            .get()
            .and_then(|index| {
                form.mapping_camera
                    .get()
                    .and_then(|id| camera_summary(&index, &id))
            })
            .unwrap_or_default(),
        ProposalKind::Body => (form.body_make.get(), form.body_model.get()),
        _ => (String::new(), String::new()),
    }
}

fn submission_identity(
    index: Option<&SiteIndex>,
    validated: &ValidatedPayload,
) -> (String, String) {
    match validated {
        ValidatedPayload::Body(body) => (body.make.clone(), body.model.clone()),
        ValidatedPayload::Mapping(mapping) => index
            .and_then(|index| camera_summary(index, &mapping.body))
            .unwrap_or_default(),
        _ => (String::new(), String::new()),
    }
}

fn fixture_camera_matches(form: &Form, validated: &ValidatedPayload) -> bool {
    match validated {
        ValidatedPayload::Body(body) => {
            let make = form.body_make.get_untracked();
            let model = form.body_model.get_untracked();
            body.make == make.trim()
                && body.model == model.trim()
                && looklab_schema::body_id_from_name(&make, &model).is_ok_and(|id| id == body.id)
        }
        ValidatedPayload::Mapping(mapping) => {
            form.mapping_camera.get_untracked().as_ref() == Some(&mapping.body)
        }
        _ => false,
    }
}

fn chosen_fixtures(form: &Form) -> Vec<FixtureFile> {
    form.fixture_files
        .get()
        .into_iter()
        .enumerate()
        .filter(|(index, _)| form.consented.get().get(*index).copied().unwrap_or(false))
        .map(|(_, file)| file)
        .collect()
}

fn pending_matches(form: &Form, pending: &PendingDonations) -> bool {
    if form.kind.get_untracked() != pending.kind {
        return false;
    }
    let Some(current) = form
        .manual
        .get_untracked()
        .or_else(|| computed_payload(form).ok())
    else {
        return false;
    };
    if current != pending.payload {
        return false;
    }
    let Some(validated) = validate_proposal(pending.kind, &current).ok() else {
        return false;
    };
    if !fixture_camera_matches(form, &validated) {
        return false;
    }
    let (make, model) = form
        .index
        .with_untracked(|index| submission_identity(index.as_ref(), &validated));
    if make != pending.make || model != pending.model {
        return false;
    }
    form.fixture_files.with_untracked(|files| {
        form.consented.with_untracked(|consented| {
            let mut chosen = 0;
            for (position, file) in files.iter().enumerate() {
                if !consented.get(position).copied().unwrap_or(false) {
                    continue;
                }
                let Some(saved) = pending.files.get(chosen) else {
                    return false;
                };
                if file.name != saved.name || file.sha256 != saved.sha256 {
                    return false;
                }
                chosen += 1;
            }
            chosen == pending.files.len()
        })
    })
}

fn collision_error(index: &SiteIndex, validated: &ValidatedPayload) -> Option<String> {
    match validated {
        ValidatedPayload::Body(body) => index
            .bodies
            .iter()
            .any(|camera| camera.id == body.id)
            .then(|| {
                format!(
                    "The site already lists {} {} at /cameras/{}. Propose a change to that camera instead.",
                    body.make, body.model, body.id
                )
            }),
        ValidatedPayload::LutLook(look) => index
            .looks
            .iter()
            .any(|published| {
                published.slug.as_str() == look.look.as_str() && !published.versions.is_empty()
            })
            .then(|| {
                format!(
                    "The link /looks/{} already has published versions. Choose another name.",
                    look.look
                )
            }),
        _ => None,
    }
}

fn load_camera(form: Form, id: BodyId, for_schema: bool) {
    let make = id.make().to_owned();
    let model = id.model().to_owned();
    let request = if for_schema {
        form.schema_request
    } else {
        form.mapping_request
    };
    let token = request.get_untracked().wrapping_add(1);
    request.set(token);
    form.manual.set(None);
    form.reset_evidence();
    if for_schema {
        form.schema_page.set(None);
        form.schema_error.set(None);
        form.schema_selected.set(Vec::new());
        form.schema_edits.set(Vec::new());
        form.first_schema_camera.set(None);
        form.first_schema_text.set(String::new());
        form.first_schema_error.set(None);
        form.first_schema_generation
            .update(|read| *read = read.supersede());
    } else {
        form.mapping_schema.set(None);
        form.mapping_error.set(None);
        form.mapping_existing.set(None);
    }
    spawn_local(async move {
        let result = async {
            let page = data::body(&make, &model).await?;
            let schema = looklab_schema::validate_control_schema(&page.schema)
                .map_err(|errors| join_errors(&errors))?;
            Ok::<_, String>((page, schema))
        }
        .await;
        if request.try_get_untracked() != Some(token) {
            return;
        }
        match result {
            Ok((page, schema)) => {
                if for_schema {
                    form.schema_page.set(Some(page));
                    form.schema_error.set(None);
                    form.schema_epoch
                        .set(schema.epochs.first().map(|epoch| epoch.id).unwrap_or(1));
                } else {
                    let epoch = schema.epochs.first().map(|epoch| epoch.id).unwrap_or(1);
                    form.mapping_schema.set(Some(schema));
                    form.mapping_error.set(None);
                    form.mapping_epoch.set(epoch);
                }
            }
            Err(error) => {
                if for_schema {
                    form.schema_error.set(Some(error));
                } else {
                    form.mapping_error.set(Some(error));
                }
            }
        }
    });
}

fn clear_camera(form: Form, for_schema: bool) {
    let request = if for_schema {
        form.schema_request
    } else {
        form.mapping_request
    };
    request.set(request.get_untracked().wrapping_add(1));
    form.manual.set(None);
    form.reset_evidence();
    if for_schema {
        form.schema_camera.set(None);
        form.schema_page.set(None);
        form.schema_error.set(None);
        form.schema_selected.set(Vec::new());
        form.schema_edits.set(Vec::new());
    } else {
        form.mapping_camera.set(None);
        form.mapping_schema.set(None);
        form.mapping_error.set(None);
        form.mapping_existing.set(None);
    }
}

fn load_first_schema(form: Form, id: BodyId) {
    form.manual.set(None);
    form.reset_evidence();
    form.schema_request
        .set(form.schema_request.get_untracked().wrapping_add(1));
    form.first_schema_generation
        .update(|read| *read = read.supersede());
    form.schema_camera.set(None);
    form.schema_page.set(None);
    form.schema_error.set(None);
    form.schema_selected.set(Vec::new());
    form.schema_edits.set(Vec::new());
    form.first_schema_camera.set(Some(id));
    form.first_schema_text.set(String::new());
    form.first_schema_error.set(None);
}

fn clear_first_schema(form: Form) {
    form.manual.set(None);
    form.reset_evidence();
    form.first_schema_generation
        .update(|read| *read = read.supersede());
    form.first_schema_camera.set(None);
    form.first_schema_text.set(String::new());
    form.first_schema_error.set(None);
}

#[component]
pub fn ProposePage() -> impl IntoView {
    let query = leptos_router::hooks::use_query_map();
    let initial = query
        .get_untracked()
        .get("kind")
        .and_then(|value| value.parse::<ProposalKind>().ok())
        .filter(|kind| {
            matches!(
                kind,
                ProposalKind::Body
                    | ProposalKind::Mapping
                    | ProposalKind::SchemaChange
                    | ProposalKind::LutLook
                    | ProposalKind::InterfaceProfile
            )
        })
        .unwrap_or(ProposalKind::Mapping);
    let form = Form::new(initial);
    let session = crate::session::use_session();
    spawn_local(async move {
        match data::index().await {
            Ok(index) => form.index.set(Some(index)),
            Err(error) => form.index_error.set(Some(error)),
        }
    });
    spawn_local(async move {
        if !crate::base::server_available() {
            return;
        }
        match data::bodies_without_schema().await {
            Ok(bodies) => form.body_context.set(Some(bodies)),
            Err(error) => form.body_context_error.set(Some(error)),
        }
    });
    let computed = Memo::new(move |_| computed_payload(&form));
    let payload = Signal::derive(move || {
        form.manual
            .get()
            .unwrap_or_else(|| computed.get().unwrap_or_default())
    });
    let submit = move |_| {
        if form.busy.get() {
            return;
        }
        form.pending.set(None);
        let kind = form.kind.get();
        let current = match form.manual.get_untracked() {
            Some(text) => text,
            None => match computed_payload(&form) {
                Ok(text) => text,
                Err(error) => {
                    form.message.set(Some(error));
                    return;
                }
            },
        };
        let validated = match validate_proposal(kind, &current) {
            Ok(validated) => validated,
            Err(errors) => {
                form.message.set(Some(format!(
                    "Check these details before sending: {}",
                    join_errors(&errors)
                )));
                return;
            }
        };
        let index = form.index.get_untracked();
        if let Some(index) = index.as_ref()
            && let Some(error) = collision_error(index, &validated)
        {
            form.message.set(Some(error));
            return;
        }
        let lut_file = if kind == ProposalKind::LutLook {
            let Some(file) = form.lut_file.get_untracked() else {
                form.message.set(Some("Choose a .cube file.".to_owned()));
                return;
            };
            let ValidatedPayload::LutLook(look) = &validated else {
                form.message
                    .set(Some("Check these details before sending.".to_owned()));
                return;
            };
            if look.lut != file.file() {
                form.message.set(Some(
                    "The Advanced payload describes a different .cube file than the selected attachment. Choose the file again or reset the form values.".to_owned(),
                ));
                return;
            }
            Some(file)
        } else {
            None
        };
        let evidence_relevant = match &validated {
            ValidatedPayload::Mapping(_) => true,
            ValidatedPayload::Body(body) => body.kind == looklab_schema::BodyKind::Camera,
            _ => false,
        };
        let chosen = if evidence_relevant {
            chosen_fixtures(&form)
        } else {
            Vec::new()
        };
        if !chosen.is_empty() && !fixture_camera_matches(&form, &validated) {
            form.message.set(Some(
                "The Advanced payload names a different camera than the evidence selection. Reset the form values or select that camera before donating files.".to_owned(),
            ));
            return;
        }
        let fixture_backed = matches!(
            &validated,
            ValidatedPayload::Body(body)
                if body.kind == looklab_schema::BodyKind::Camera
                    && body.mapping_status == MappingStatus::FixtureBacked
        );
        if fixture_backed && chosen.is_empty() {
            form.message.set(Some(
                "A camera confirmed from a sample file needs at least one donated original file. Choose provisional without one.".to_owned(),
            ));
            return;
        }
        let (make, model) = submission_identity(index.as_ref(), &validated);
        form.message.set(None);
        form.submitted.set(None);
        if chosen.is_empty() {
            submit_now(form, session, kind, current, Vec::new(), lut_file);
        } else {
            form.pending.set(Some(PendingDonations {
                files: chosen,
                make,
                model,
                kind,
                payload: current,
                lut_file,
            }));
        }
    };
    let confirm_donations = move |_| {
        let Some(pending) = form.pending.get() else {
            return;
        };
        if !pending_matches(&form, &pending) {
            form.pending.set(None);
            form.message.set(Some(
                "The proposal or its evidence changed after you reviewed the files. Submit again."
                    .to_owned(),
            ));
            return;
        }
        form.busy.set(true);
        form.message.set(None);
        spawn_local(async move {
            if session.require_profile().await.is_none() {
                form.busy.set(false);
                return;
            }
            let mut donations = Vec::with_capacity(pending.files.len());
            for file in &pending.files {
                let source = match data::upload_fixture(session, file).await {
                    Ok(source) => source,
                    Err(error) => {
                        form.message.set(Some(error));
                        form.busy.set(false);
                        return;
                    }
                };
                match proposal_form::donation(file, &pending.make, &pending.model, source) {
                    Ok(donation) => donations.push(donation),
                    Err(error) => {
                        form.message.set(Some(error));
                        form.busy.set(false);
                        form.pending.set(None);
                        return;
                    }
                }
            }
            let lut_file = pending.lut_file;
            let result = if pending.kind == ProposalKind::LutLook {
                match lut_file {
                    Some(file) => {
                        data::submit_lut_proposal(
                            session,
                            pending.kind,
                            &pending.payload,
                            &donations,
                            &file,
                        )
                        .await
                    }
                    None => Err("Choose a .cube file.".to_owned()),
                }
            } else {
                data::submit_proposal(session, pending.kind, &pending.payload, &donations).await
            };
            match result {
                Ok(reply) => {
                    form.submitted.set(Some(reply.view));
                    form.pending.set(None);
                }
                Err(error) => form.message.set(Some(error)),
            }
            form.busy.set(false);
        });
    };
    let cancel_donations = move |_| form.pending.set(None);
    let available = crate::base::server_available();

    view! {
        <h1>"Propose a change"</h1>
        <p class="lede">"Tell us what your camera can do. A moderator checks the change and the evidence you share."</p>
        {move || form.index_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
        <KindPicker form/>
        {move || form.index.get().map(|index| {
            let cameras = index.bodies.clone();
            let mapping_cameras = cameras.clone();
            let schema_cameras = cameras;
            let body_index = index.clone();
            let lut_index = index;
            view! {
                <div class="proposal-fields">
                    {move || (form.kind.get() == ProposalKind::Body).then(|| view! { <BodyFields form index=body_index.clone()/> })}
                    {move || (form.kind.get() == ProposalKind::Mapping).then(|| view! { <MappingFields form cameras=mapping_cameras.clone()/> })}
                    {move || (form.kind.get() == ProposalKind::SchemaChange).then(|| view! { <SchemaFields form cameras=schema_cameras.clone()/> })}
                    {move || (form.kind.get() == ProposalKind::LutLook).then(|| view! { <LutFields form index=lut_index.clone()/> })}
                    {move || (form.kind.get() == ProposalKind::InterfaceProfile).then(|| view! { <InterfaceFields form/> })}
                    {move || matches!(form.kind.get(), ProposalKind::Body | ProposalKind::Mapping).then(|| view! { <EvidenceFields form/> })}
                </div>
            }
        })}
        <section aria-labelledby="submit">
            <h2 id="submit">"Submit"</h2>
            <p>"A moderator reviews your proposal before it changes the site."</p>
            {(!available).then(crate::ui::preview_notice)}
            <button type="button" id="submit-proposal" disabled=move || form.busy.get() || !available on:click=submit>
                {move || if form.busy.get() { "Preparing proposal…" } else { "Submit proposal" }}
            </button>
            {move || form.pending.get().map(|selected| {
                let rows = selected.files.iter().map(|file| view! {
                    <tr>
                        <th scope="row">{file.name.clone()}</th>
                        <td>{file.bytes.len().to_string()}</td>
                        <td>{file.sha256.to_hex()}</td>
                    </tr>
                }).collect_view();
                let count = selected.files.len();
                view! {
                    <div class="valid" id="donation-confirm">
                        <h3>"Files that leave this device"</h3>
                        <p>{format!("These {count} file(s) are uploaded now, stored privately, and published as CC0-1.0 fixtures only if a moderator approves this proposal.")}</p>
                        <table class="data-table">
                            <caption>"Donated files"</caption>
                            <thead><tr><th scope="col">"File"</th><th scope="col">"Bytes"</th><th scope="col">"sha256"</th></tr></thead>
                            <tbody>{rows}</tbody>
                        </table>
                        <div class="field-row">
                            <button type="button" id="confirm-donation" disabled=move || form.busy.get() || !available on:click=confirm_donations>"Upload files and submit"</button>
                            <button type="button" id="cancel-donation" disabled=move || form.busy.get() on:click=cancel_donations>"Cancel"</button>
                        </div>
                    </div>
                }
            })}
            {move || form.message.get().map(|text| view! { <p class="error" role="alert">{text}</p> })}
            {move || form.submitted.get().map(|proposal| {
                let href = crate::base::site_url(&format!("/proposals/{}", proposal.id.as_str()));
                view! {
                    <div class="valid">
                        <p>"Submitted. It is now in the review queue."</p>
                        <a id="submitted-link" href=href>"Open the proposal"</a>
                    </div>
                }
            })}
        </section>
        <Preview form/>
        <Advanced form payload/>
    }
}

fn submit_now(
    form: Form,
    session: crate::session::SessionContext,
    kind: ProposalKind,
    payload: String,
    donations: Vec<looklab_schema::FixtureDonation>,
    lut_file: Option<proposal_form::LutFileDraft>,
) {
    form.busy.set(true);
    spawn_local(async move {
        if session.require_profile().await.is_none() {
            form.busy.set(false);
            return;
        }
        let result = if kind == ProposalKind::LutLook {
            match lut_file {
                Some(file) => {
                    data::submit_lut_proposal(session, kind, &payload, &donations, &file).await
                }
                None => Err("Choose a .cube file.".to_owned()),
            }
        } else {
            data::submit_proposal(session, kind, &payload, &donations).await
        };
        match result {
            Ok(reply) => form.submitted.set(Some(reply.view)),
            Err(error) => form.message.set(Some(error)),
        }
        form.busy.set(false);
    });
}

#[component]
fn KindPicker(form: Form) -> impl IntoView {
    let kinds = [
        ProposalKind::Body,
        ProposalKind::Mapping,
        ProposalKind::SchemaChange,
        ProposalKind::LutLook,
        ProposalKind::InterfaceProfile,
    ];
    let description = move || match form.kind.get() {
        ProposalKind::Body => {
            "Add a camera the site does not list yet. It appears under Cameras once approved."
        }
        ProposalKind::Mapping => {
            "Add or fix a film simulation in a camera's menu. Get the look and the camera page use these mappings to read settings from your photos."
        }
        ProposalKind::SchemaChange => {
            "Correct the controls a camera offers, their ranges or menu choices. Get the look uses them to write settings cards."
        }
        ProposalKind::LutLook => {
            "Add a custom look built from a .cube LUT file. The file is uploaded with the proposal and stays private until approval; published custom looks appear on Looks and in Compare, and matchable ones also appear in Get the look."
        }
        ProposalKind::InterfaceProfile => {
            "Define how an editing app applies exported looks with a looklab YAML profile. Published profiles appear on For developers → Editing apps and enable the Editing app result in Get the look."
        }
        _ => "",
    };
    view! {
        <section aria-labelledby="kind-heading">
            <h2 id="kind-heading">"What are you proposing?"</h2>
            <select id="kind" aria-labelledby="kind-heading" on:change=move |event| {
                if let Ok(next) = event_target_value(&event).parse::<ProposalKind>() {
                    form.reset_for_kind(next);
                }
            }>
                {kinds.into_iter().map(|candidate| view! {
                    <option value=candidate.as_str() prop:selected=move || form.kind.get() == candidate>
                        {proposal_form::kind_label(candidate)}
                    </option>
                }).collect_view()}
            </select>
            <p class="muted">{description}</p>
        </section>
    }
}

#[component]
fn BodyFields(form: Form, index: SiteIndex) -> impl IntoView {
    let collision = StoredValue::new(index);
    view! {
        <section aria-labelledby="body-heading">
            <h2 id="body-heading">"The camera"</h2>
            <label for="body-make">"Make"</label>
            <input id="body-make" prop:value=move || form.body_make.get() on:input=move |event| { form.manual.set(None); form.reset_evidence(); form.body_make.set(event_target_value(&event)); }/>
            <label for="body-model">"Model"</label>
            <input id="body-model" prop:value=move || form.body_model.get() on:input=move |event| { form.manual.set(None); form.reset_evidence(); form.body_model.set(event_target_value(&event)); }/>
            <p class="muted">{move || looklab_schema::body_id_from_name(&form.body_make.get(), &form.body_model.get()).map(|id| format!("Link: /cameras/{id}")).unwrap_or_default()}</p>
            {move || {
                let id = looklab_schema::body_id_from_name(&form.body_make.get(), &form.body_model.get()).ok()?;
                collision.with_value(|index| index.bodies.iter().any(|camera| camera.id == id)).then(|| view! {
                    <p class="error" role="alert">{format!("The site already lists /cameras/{id}. Propose a change to that camera instead.")}</p>
                })
            }}
            <label for="body-kind">"Kind"</label>
            <select id="body-kind" on:change=move |event| {
                if let Ok(choice) = event_target_value(&event).parse::<BodyKindChoice>() {
                    form.manual.set(None);
                    form.reset_evidence();
                    form.body_kind.set(choice.body_kind());
                    if choice == BodyKindChoice::Software {
                        form.body_mapping.set(MappingStatus::Provisional);
                    }
                }
            }>
                <option value=BodyKindChoice::Camera.to_string() prop:selected=move || form.body_kind.get() == looklab_schema::BodyKind::Camera>"Camera"</option>
                <option value=BodyKindChoice::Software.to_string() prop:selected=move || form.body_kind.get() == looklab_schema::BodyKind::Software>"Editing app"</option>
            </select>
            {move || (form.body_kind.get() == looklab_schema::BodyKind::Camera).then(|| view! {
                <div>
                    <label for="body-mapping">"Sample-file status"</label>
                    <select id="body-mapping" on:change=move |event| {
                        if let Ok(choice) = event_target_value(&event).parse::<MappingStatusChoice>() {
                            form.manual.set(None);
                            form.body_mapping.set(choice.mapping_status());
                        }
                    }>
                        <option value=MappingStatusChoice::Provisional.to_string() prop:selected=move || form.body_mapping.get() == MappingStatus::Provisional>"Not yet confirmed from a sample file"</option>
                        <option value=MappingStatusChoice::FixtureBacked.to_string() prop:selected=move || form.body_mapping.get() == MappingStatus::FixtureBacked>"Confirmed from a sample file"</option>
                    </select>
                </div>
            })}
            <label class="consent"><input type="checkbox" prop:checked=move || form.body_raw.get() on:change=move |event| { form.manual.set(None); form.body_raw.set(event_target_checked(&event)); }/>"Raw conversion in the camera"</label>
            <label class="consent"><input type="checkbox" prop:checked=move || form.body_heif.get() on:change=move |event| { form.manual.set(None); form.body_heif.set(event_target_checked(&event)); }/>"HEIF files"</label>
            <label for="body-spaces">"JPEG color spaces"</label>
            <input id="body-spaces" prop:value=move || form.body_spaces.get() on:input=move |event| { form.manual.set(None); form.body_spaces.set(event_target_value(&event)); }/>
            <label for="body-aliases">"EXIF model aliases (one per line)"</label>
            <textarea id="body-aliases" rows="3" prop:value=move || form.body_aliases.get() on:input=move |event| { form.manual.set(None); form.body_aliases.set(event_target_value(&event)); }></textarea>
        </section>
    }
}

#[component]
fn MappingFields(form: Form, cameras: Vec<looklab_publish::BodySummary>) -> impl IntoView {
    view! {
        <section aria-labelledby="mapping-heading">
            <h2 id="mapping-heading">"The film simulation"</h2>
            <CameraSelect cameras selected=form.mapping_camera id="mapping-camera".to_owned() label="Camera".to_owned() on_change=Callback::new(move |camera: Option<BodyId>| match camera { Some(id) => load_camera(form, id, false), None => clear_camera(form, false) })/>
            {move || form.mapping_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
            {move || form.mapping_schema.get().map(|schema| {
                let epochs = schema.epochs.iter().map(|epoch| (epoch.id, epoch.firmware.from.to_string(), epoch.firmware.to.to_string())).collect::<Vec<_>>();
                let sims = schema.epochs.iter().flat_map(|epoch| epoch.native_sims.iter().map(move |sim| (epoch.id, sim.key.clone(), sim.look.clone(), sim.monochrome))).collect::<Vec<_>>();
                let sims_for_change = StoredValue::new(sims.clone());
                let epochs_for_change = StoredValue::new(epochs.clone());
                let suggestion_schema = schema.clone();
                view! {
                    <div class="field-row">
                        <label><input type="radio" name="mapping-mode" prop:checked=move || form.mapping_mode.get() == MappingMode::New on:change=move |_| { form.manual.set(None); form.mapping_mode.set(MappingMode::New); }/>"Add a film simulation"</label>
                        <label><input type="radio" name="mapping-mode" prop:checked=move || form.mapping_mode.get() == MappingMode::Existing on:change=move |_| { form.manual.set(None); form.mapping_mode.set(MappingMode::Existing); }/>"Correct when an existing simulation became available"</label>
                    </div>
                    {move || {
                        let suggestion_schema = suggestion_schema.clone();
                        (form.mapping_mode.get() == MappingMode::New).then(move || view! {
                        <div>
                            <label for="mapping-name">"Film simulation in the camera menu"</label>
                            <input id="mapping-name" prop:value=move || form.mapping_name.get() on:input=move |event| { form.manual.set(None); form.mapping_name.set(event_target_value(&event)); }/>
                            <p class="muted">{move || looklab_schema::look_id_from_name(&form.mapping_name.get()).map(|look| format!("Link: /looks/{look}")).unwrap_or_default()}</p>
                            {move || {
                                let suggestions = proposal_form::suggest_sims(&suggestion_schema, &form.mapping_name.get());
                                suggestions.into_iter().map(|suggestion| {
                                let identity = suggestion.identity.clone();
                                let firmware_from = suggestion.firmware_from.clone();
                                let firmware_to = suggestion.firmware_to.clone();
                                let look = suggestion.look.clone();
                                view! {
                                    <div class="valid">
                                        <p>{format!("This camera already lists {} as {} in firmware {} to {} (epoch {}).", identity.key, look, firmware_from, firmware_to, identity.epoch)}</p>
                                        <button type="button" on:click={
                                            let identity = identity.clone();
                                            move |_| {
                                                form.manual.set(None);
                                                form.mapping_mode.set(MappingMode::Existing);
                                                form.mapping_existing.set(Some(identity.clone()));
                                                epochs_for_change.with_value(|epochs| {
                                                    if let Some((id, _, _)) = epochs.iter().find(|(id, _, _)| *id != identity.epoch) {
                                                        form.mapping_epoch.set(*id);
                                                    }
                                                });
                                            }
                                        }>"Use the existing simulation"</button>
                                    </div>
                                }
                                }).collect_view()
                            }}
                        </div>
                        })
                    }}
                    {move || (form.mapping_mode.get() == MappingMode::Existing).then(|| view! {
                        <div>
                            <label for="mapping-existing">"Existing simulation"</label>
                            <select id="mapping-existing" on:change=move |event| {
                                form.manual.set(None);
                                let Ok(index) = event_target_value(&event).parse::<usize>() else {
                                    form.mapping_existing.set(None);
                                    return;
                                };
                                sims_for_change.with_value(|sims| {
                                    if let Some((epoch, key, _, _)) = sims.get(index) {
                                        let identity = SimIdentity { epoch: *epoch, key: key.clone() };
                                        form.mapping_existing.set(Some(identity.clone()));
                                        if form.mapping_epoch.get() == identity.epoch {
                                            epochs_for_change.with_value(|epochs| {
                                                if let Some((id, _, _)) = epochs.iter().find(|(id, _, _)| *id != identity.epoch) {
                                                    form.mapping_epoch.set(*id);
                                                }
                                            });
                                        }
                                    }
                                });
                            }>
                                <option value="">"Choose a simulation"</option>
                                {sims.iter().enumerate().map(|(index, (epoch, key, look, _))| {
                                    let choice_epoch = *epoch;
                                    let choice_key = key.clone();
                                    let label = format!("{key} · {look} · epoch {epoch}");
                                    view! {
                                        <option value=index.to_string() data-epoch=epoch.to_string() prop:selected=move || form.mapping_existing.get().as_ref().is_some_and(|selected| selected.epoch == choice_epoch && selected.key == choice_key)>
                                            {label}
                                        </option>
                                    }
                                }).collect_view()}
                            </select>
                            <p class="muted">"The existing menu value and Look link are kept. Choose the firmware range where the simulation actually appeared."</p>
                        </div>
                    })}
                    <label for="mapping-epoch">"Firmware range"</label>
                    <select id="mapping-epoch" on:change=move |event| {
                        form.manual.set(None);
                        if let Ok(epoch) = event_target_value(&event).parse::<u32>() { form.mapping_epoch.set(epoch); }
                    }>
                        {epochs.into_iter().map(|(id, from, to)| view! {
                            <option value=id.to_string() prop:selected=move || form.mapping_epoch.get() == id>{format!("{from} to {to}")}</option>
                        }).collect_view()}
                    </select>
                    {move || (form.mapping_mode.get() == MappingMode::New).then(|| view! {
                        <div>
                            <label for="mapping-tag">"Makernote tag"</label>
                            <input id="mapping-tag" prop:value=move || form.mapping_tag.get() on:input=move |event| { form.manual.set(None); form.mapping_tag.set(event_target_value(&event)); }/>
                            <label for="mapping-value">"Makernote value"</label>
                            <input id="mapping-value" placeholder="0x0600" prop:value=move || form.mapping_value.get() on:input=move |event| { form.manual.set(None); form.mapping_value.set(event_target_value(&event)); }/>
                            <label class="consent"><input type="checkbox" prop:checked=move || form.mapping_monochrome.get() on:change=move |event| { form.manual.set(None); form.mapping_monochrome.set(event_target_checked(&event)); }/>"This simulation renders black and white"</label>
                        </div>
                    })}
                    <p class="muted">{move || format!("Selected firmware range: epoch {}.", form.mapping_epoch.get())}</p>
                    <crate::ui::Disclosure summary="Read the menu value from camera files".to_owned()>
                        <p>"Choose several original camera files, then label each with its film simulation. Photos stay on this device."</p>
                        <crate::ui::DropZone on_files=Callback::new(move |selected| {
                            spawn_local(async move {
                                let read = files::selected_files(selected);
                                let mut parsed = Vec::new();
                                for file in read {
                                    let bytes = match file.bytes().await {
                                        Ok(bytes) => bytes,
                                        Err(error) => { form.message.set(Some(error)); return; }
                                    };
                                    match proposal_form::file_menu_value(&bytes) {
                                        Ok(tags) => parsed.push(MenuFileDraft {
                                            name: file.name().to_owned(),
                                            answer: String::new(),
                                            parsed_values: tags.replace('\n', " "),
                                        }),
                                        Err(error) => { form.message.set(Some(error)); return; }
                                    }
                                }
                                form.mapping_labeled.set(parsed);
                            });
                        }) accept=".jpg,.jpeg,.raf".to_owned() folders=false/>
                        {move || form.mapping_labeled.get().into_iter().enumerate().map(|(index, file)| view! {
                            <label for=format!("mapping-answer-{index}")>{file.name}</label>
                            <input id=format!("mapping-answer-{index}") value=file.answer placeholder="For example: Classic Chrome" on:input=move |event| {
                                form.manual.set(None);
                                form.mapping_labeled.update(|files| files[index].answer = event_target_value(&event));
                                let text = form.mapping_labeled.get().iter().filter(|file| !file.answer.is_empty()).map(|file| format!("{} {}", file.answer.to_lowercase().replace([' ', '-'], "_"), file.parsed_values)).collect::<Vec<_>>().join("\n");
                                if let Some((tag, value)) = proposal_form::inferred_makernote(&text, &form.mapping_name.get()) {
                                    form.mapping_tag.set(tag.to_string());
                                    form.mapping_value.set(format!("0x{value:04x}"));
                                }
                                form.mapping_files_text.set(text);
                            }/>
                        }).collect_view()}
                        <label for="mapping-files">"One file per line: answer Tag=value ..."</label>
                        <textarea id="mapping-files" rows="5" cols="60" prop:value=move || form.mapping_files_text.get() on:input=move |event| { form.mapping_files_text.set(event_target_value(&event)); }></textarea>
                        {move || match proposal_form::parse_labeled_files(&form.mapping_files_text.get()) {
                            Ok(inference) => {
                                let rows = inference.candidates.iter().map(|candidate| {
                                    let pairs = candidate.pairs.iter().map(|(label, value)| format!("{label}: {:04x}", value.0)).collect::<Vec<_>>().join(", ");
                                    view! { <tr><th scope="row">{candidate.tag.to_string()}</th><td>{if candidate.consistent { "yes" } else { "no" }}</td><td>{format!("{:.0}%", candidate.confidence * 100.0)}</td><td>{pairs}</td></tr> }
                                }).collect_view();
                                let best = inference.best().map(|candidate| format!("Use {} = the value that differs per answer.", candidate.tag));
                                view! {
                                    <p class="muted">{looklab_schema::MENU_QUESTION}</p>
                                    <table class="data-table"><caption>"Candidate tags"</caption>
                                    <thead><tr><th scope="col">"Tag"</th><th scope="col">"Separates the answers"</th><th scope="col">"Confidence"</th><th scope="col">"Values"</th></tr></thead>
                                    <tbody>{rows}</tbody></table>
                                    {best.unwrap_or_else(|| "No single tag separates the answers yet.".to_owned())}
                                }.into_any()
                            }
                            Err(error) => view! { <p class="error">{error}</p> }.into_any(),
                        }}
                    </crate::ui::Disclosure>
                }.into_any()
            })}
        </section>
    }
}

#[component]
fn SchemaFields(form: Form, cameras: Vec<looklab_publish::BodySummary>) -> impl IntoView {
    view! {
        <section aria-labelledby="schema-heading">
            <h2 id="schema-heading">"The camera controls"</h2>
            <p class="muted">"The form starts from the published control schema for this camera. Select every control you are changing; each keeps its own editor. Controls you leave alone, including optional definitions, stay exactly as published. For a registered camera without a schema, use the first-schema section."</p>
            <CameraSelect cameras selected=form.schema_camera id="schema-camera".to_owned() label="Camera".to_owned() on_change=Callback::new(move |camera: Option<BodyId>| match camera { Some(id) => load_camera(form, id, true), None => clear_camera(form, true) })/>
            {move || form.schema_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
            <crate::ui::Disclosure summary="First schema for a registered camera without one".to_owned()>
                <p class="muted">"Use this only when the camera is already registered and has no published control schema. Paste or load the complete schema; the same validation, signature and moderation apply."</p>
                {move || form.body_context_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
                <label for="first-schema-camera">"Registered camera without a schema"</label>
                <select id="first-schema-camera" on:change=move |event| {
                    match BodyId::new(&event_target_value(&event)) {
                        Ok(id) => load_first_schema(form, id),
                        Err(_) => clear_first_schema(form),
                    }
                }>
                    <option value="">"Choose a camera"</option>
                    {move || form.body_context.get().unwrap_or_default().into_iter().filter(|body| body.kind == looklab_schema::BodyKind::Camera).map(|body| {
                        let id = body.id.clone();
                        let value = id.to_string();
                        let label = format!("{} {}", body.make, body.model);
                        view! {
                            <option value=value prop:selected=move || form.first_schema_camera.get().as_ref() == Some(&id)>{label}</option>
                        }
                    }).collect_view()}
                </select>
                {move || form.first_schema_camera.get().map(|camera| view! {
                    <p class="muted">{format!("First schema for {camera}")}</p>
                    <label for="first-schema-yaml">"Complete control schema (YAML)"</label>
                    <textarea id="first-schema-yaml" rows="16" cols="80" prop:value=move || form.first_schema_text.get() on:input=move |event| {
                        form.manual.set(None);
                        form.first_schema_error.set(None);
                        form.first_schema_generation.update(|read| *read = read.supersede());
                        form.first_schema_text.set(event_target_value(&event));
                    }></textarea>
                    <crate::ui::DropZone on_files=Callback::new(move |selected| {
                        let Some(file) = files::selected_files(selected).into_iter().next() else { return; };
                        let read = form.first_schema_generation.get_untracked().supersede();
                        form.first_schema_generation.set(read);
                        form.manual.set(None);
                        let name = file.name().to_owned();
                        spawn_local(async move {
                            let bytes = match file.bytes().await {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    if form.first_schema_read_is_current(read) {
                                        form.first_schema_error.set(Some(error));
                                    }
                                    return;
                                }
                            };
                            if !form.first_schema_read_is_current(read) {
                                return;
                            }
                            match String::from_utf8(bytes) {
                                Ok(text) => match looklab_schema::validate_control_schema(&text) {
                                    Ok(schema) => {
                                        form.first_schema_text.set(text);
                                        if form.first_schema_camera.get_untracked().as_ref() == Some(&schema.body) {
                                            form.first_schema_error.set(None);
                                        } else {
                                            form.first_schema_error.set(Some(format!("The schema names {}, not the selected camera.", schema.body)));
                                        }
                                    }
                                    Err(errors) => form.first_schema_error.set(Some(join_errors(&errors))),
                                },
                                Err(_) => form.first_schema_error.set(Some(format!("{name} is not a UTF-8 YAML file."))),
                            }
                        });
                    }) accept=".yaml,.yml".to_owned() folders=false/>
                    {move || form.first_schema_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
                })}
            </crate::ui::Disclosure>
            {move || form.schema_page.get().map(|page| {
                let schema = looklab_schema::validate_control_schema(&page.schema).ok();
                let epochs = schema.as_ref().map(|schema| schema.epochs.iter().map(|epoch| (epoch.id, epoch.firmware.from.to_string(), epoch.firmware.to.to_string())).collect::<Vec<_>>()).unwrap_or_default();
                let controls = schema.as_ref().and_then(|schema| schema.epochs.iter().find(|epoch| epoch.id == form.schema_epoch.get())).map(|epoch| epoch.controls.clone()).unwrap_or_default();
                let controls_for_editors = controls.clone();
                let selected_controls = Signal::derive(move || {
                    let selected = form.schema_selected.get();
                    controls_for_editors
                        .iter()
                        .filter(|control| selected.contains(&control.key))
                        .cloned()
                        .collect::<Vec<_>>()
                });
                view! {
                    <label for="schema-epoch">"Firmware range"</label>
                    <select id="schema-epoch" on:change=move |event| {
                        form.manual.set(None);
                        if let Ok(epoch) = event_target_value(&event).parse::<u32>() {
                            form.schema_epoch.set(epoch);
                            form.schema_selected.set(Vec::new());
                        }
                    }>
                        {epochs.into_iter().map(|(id, from, to)| view! {
                            <option value=id.to_string() prop:selected=move || form.schema_epoch.get() == id>{format!("{from} to {to}")}</option>
                        }).collect_view()}
                    </select>
                    <fieldset>
                        <legend>"Controls to change"</legend>
                        {controls.iter().map(|control| {
                            let key = control.key.clone();
                            let checked = move || form.schema_selected.get().contains(&key);
                            let toggle_key = control.key.clone();
                            view! {
                                <label class="consent">
                                    <input type="checkbox" prop:checked=checked on:change=move |event| {
                                        let checked = event_target_checked(&event);
                                        let key = toggle_key.clone();
                                        form.manual.set(None);
                                        form.schema_selected.update(|selected| {
                                            if checked {
                                                if !selected.contains(&key) {
                                                    selected.push(key.clone());
                                                }
                                            } else {
                                                selected.retain(|candidate| candidate != &key);
                                            }
                                        });
                                    }/>
                                    {format!("{} · {}", control.key, proposal_form::control_kind(control.kind))}
                                </label>
                            }
                        }).collect_view()}
                    </fieldset>
                    <For
                        each=move || selected_controls.get()
                        key=|control| control.key.clone()
                        children=move |control| view! { <ControlEditor form control=control/> }
                    />
                    <crate::ui::Disclosure summary="Add a control this camera does not list".to_owned()>
                        <p class="muted">"This draft starts empty. It does not reuse the editor above or the selected control's values."</p>
                        <label for="add-key">"Control key"</label>
                        <input id="add-key" prop:value=move || form.add_key.get() on:input=move |event| { form.manual.set(None); form.add_key.set(event_target_value(&event)); }/>
                        <label for="add-type">"Type"</label>
                        <select id="add-type" on:change=move |event| {
                            if let Ok(choice) = event_target_value(&event).parse::<ControlTypeChoice>() {
                                form.manual.set(None);
                                form.add_type.set(choice.control_type());
                            }
                        }>
                            <option value=ControlTypeChoice::Int.to_string() prop:selected=move || form.add_type.get() == ControlType::Int>"Whole numbers"</option>
                            <option value=ControlTypeChoice::Decimal.to_string() prop:selected=move || form.add_type.get() == ControlType::Decimal>"Decimals"</option>
                            <option value=ControlTypeChoice::Enum.to_string() prop:selected=move || form.add_type.get() == ControlType::Enum>"Menu choices"</option>
                            <option value=ControlTypeChoice::Bool.to_string() prop:selected=move || form.add_type.get() == ControlType::Bool>"On or off"</option>
                        </select>
                        {move || matches!(form.add_type.get(), ControlType::Int | ControlType::Decimal).then(|| view! {
                            <div class="field-row">
                                <label for="add-min">"Minimum"</label>
                                <input id="add-min" prop:value=move || form.add_min.get() on:input=move |event| { form.manual.set(None); form.add_min.set(event_target_value(&event)); }/>
                                <label for="add-max">"Maximum"</label>
                                <input id="add-max" prop:value=move || form.add_max.get() on:input=move |event| { form.manual.set(None); form.add_max.set(event_target_value(&event)); }/>
                                <label for="add-step">"Step"</label>
                                <input id="add-step" prop:value=move || form.add_step.get() on:input=move |event| { form.manual.set(None); form.add_step.set(event_target_value(&event)); }/>
                            </div>
                        })}
                        {move || (form.add_type.get() == ControlType::Enum).then(|| view! {
                            <div>
                                <label for="add-values">"Menu choices"</label>
                                <input id="add-values" prop:value=move || form.add_values.get() on:input=move |event| { form.manual.set(None); form.add_values.set(event_target_value(&event)); }/>
                            </div>
                        })}
                        <label for="add-default">"Default"</label>
                        <input id="add-default" prop:value=move || form.add_default.get() on:input=move |event| { form.manual.set(None); form.add_default.set(event_target_value(&event)); }/>
                        <label for="add-affects">"Affects (optional)"</label>
                        <input id="add-affects" prop:value=move || form.add_affects.get() on:input=move |event| { form.manual.set(None); form.add_affects.set(event_target_value(&event)); }/>
                        <label for="add-tolerated">"Tolerated values (optional)"</label>
                        <input id="add-tolerated" prop:value=move || form.add_tolerated.get() on:input=move |event| { form.manual.set(None); form.add_tolerated.set(event_target_value(&event)); }/>
                        <label class="consent"><input type="checkbox" prop:checked=move || form.add_look.get() on:change=move |event| { form.manual.set(None); form.add_look.set(event_target_checked(&event)); }/>"Changes the look"</label>
                        <label class="consent"><input type="checkbox" prop:checked=move || form.add_spatial.get() on:change=move |event| { form.manual.set(None); form.add_spatial.set(event_target_checked(&event)); }/>"Spatial"</label>
                        <button type="button" id="add-control" on:click=move |_| {
                            match add_control(&form) {
                                Ok(definition) => {
                                    form.manual.set(None);
                                    form.schema_edits.update(|rows| rows.push(SchemaEditRow {
                                        epoch: form.schema_epoch.get(),
                                        label: format!("{} · new control {}", form.schema_epoch.get(), definition.key),
                                        change: ControlChange::Add(definition),
                                    }));
                                }
                                Err(error) => form.message.set(Some(error)),
                            }
                        }>"Add control"</button>
                    </crate::ui::Disclosure>
                    <h3>"Changes to submit"</h3>
                    {move || form.schema_edits.get().into_iter().enumerate().map(|(index, row)| view! {
                        <div class="field-row">
                            <span>{format!("{}: {}", row.label, describe_change(&row.change))}</span>
                            <button type="button" on:click=move |_| { form.manual.set(None); form.schema_edits.update(|rows| { rows.remove(index); }); }>"Remove"</button>
                        </div>
                    }).collect_view()}
                }.into_any()
            })}
        </section>
    }
}

#[component]
fn ControlEditor(form: Form, control: Control) -> impl IntoView {
    let draft = ControlDraft::from_control(&control);
    let min = RwSignal::new(draft.min);
    let max = RwSignal::new(draft.max);
    let step = RwSignal::new(draft.step);
    let default = RwSignal::new(draft.default);
    let values = RwSignal::new(draft.values);
    let look = RwSignal::new(draft.look);
    let affects = RwSignal::new(draft.affects);
    let spatial = RwSignal::new(draft.spatial);
    let tolerated = RwSignal::new(draft.tolerated);
    let key = control.key.clone();
    let current = control.clone();
    view! {
        <div class="proposal-fields">
            <h3>{format!("Change {}", key)}</h3>
            <p class="muted">{format!("Current: {}", looklab_schema::describe_control(&control))}</p>
            <div class="field-row">
                <label for=format!("edit-{key}-min")>"Minimum"</label>
                <input id=format!("edit-{key}-min") prop:value=move || min.get() on:input=move |event| { form.manual.set(None); min.set(event_target_value(&event)); }/>
                <label for=format!("edit-{key}-max")>"Maximum"</label>
                <input id=format!("edit-{key}-max") prop:value=move || max.get() on:input=move |event| { form.manual.set(None); max.set(event_target_value(&event)); }/>
                <label for=format!("edit-{key}-step")>"Step"</label>
                <input id=format!("edit-{key}-step") prop:value=move || step.get() on:input=move |event| { form.manual.set(None); step.set(event_target_value(&event)); }/>
            </div>
            <label for=format!("edit-{key}-default")>"Default"</label>
            <input id=format!("edit-{key}-default") prop:value=move || default.get() on:input=move |event| { form.manual.set(None); default.set(event_target_value(&event)); }/>
            <label for=format!("edit-{key}-values")>"Menu choices"</label>
            <input id=format!("edit-{key}-values") prop:value=move || values.get() on:input=move |event| { form.manual.set(None); values.set(event_target_value(&event)); }/>
            <label for=format!("edit-{key}-affects")>"Affects"</label>
            <input id=format!("edit-{key}-affects") prop:value=move || affects.get() on:input=move |event| { form.manual.set(None); affects.set(event_target_value(&event)); }/>
            <label for=format!("edit-{key}-tolerated")>"Tolerated values"</label>
            <input id=format!("edit-{key}-tolerated") prop:value=move || tolerated.get() on:input=move |event| { form.manual.set(None); tolerated.set(event_target_value(&event)); }/>
            <label class="consent"><input type="checkbox" prop:checked=move || look.get() on:change=move |event| { form.manual.set(None); look.set(event_target_checked(&event)); }/>"Changes the look"</label>
            <label class="consent"><input type="checkbox" prop:checked=move || spatial.get() on:change=move |event| { form.manual.set(None); spatial.set(event_target_checked(&event)); }/>"Spatial"</label>
            <button type="button" on:click=move |_| {
                let draft = ControlDraft {
                    min: min.get(),
                    max: max.get(),
                    step: step.get(),
                    default: default.get(),
                    values: values.get(),
                    look: look.get(),
                    affects: affects.get(),
                    spatial: spatial.get(),
                    tolerated: tolerated.get(),
                };
                match diff_control(&current, &draft) {
                    Ok(edits) => {
                        form.manual.set(None);
                        let epoch = form.schema_epoch.get();
                        let empty = edits.is_empty();
                        form.schema_edits.update(|rows| {
                            rows.retain(|row| {
                                row.epoch != epoch
                                    || !matches!(&row.change, ControlChange::Set { key, .. } if key == &current.key)
                            });
                            if !empty {
                                rows.push(SchemaEditRow {
                                    epoch,
                                    label: format!("{epoch} · {}", current.key),
                                    change: ControlChange::Set { key: current.key.clone(), edits },
                                });
                            }
                        });
                        if empty {
                            form.message.set(Some("No change to add.".to_owned()));
                        }
                    }
                    Err(error) => form.message.set(Some(error)),
                }
            }>"Add this change"</button>
        </div>
    }
}

fn diff_control(control: &Control, draft: &ControlDraft) -> Result<Vec<ControlEdit>, String> {
    let mut edits = Vec::new();
    let range = if draft.min.trim().is_empty() && draft.max.trim().is_empty() {
        None
    } else {
        Some(Range {
            min: proposal_form::parse_rational(&draft.min)?,
            max: proposal_form::parse_rational(&draft.max)?,
        })
    };
    if let Some(range) = range {
        if control.range.as_ref() != Some(&range) {
            edits.push(ControlEdit::Range(range));
        }
    } else if control.range.is_some() {
        return Err("Enter both bounds; a range cannot be removed.".to_owned());
    }
    let step = if draft.step.trim().is_empty() {
        None
    } else {
        Some(proposal_form::parse_step(&draft.step)?)
    };
    if let Some(step) = step {
        if control.step != Some(step) {
            edits.push(ControlEdit::Step(step));
        }
    } else if control.step.is_some() {
        return Err("Enter a step; a step cannot be removed.".to_owned());
    }
    let default = proposal_form::parse_scalar(&draft.default, control.kind)?;
    if default != default_scalar(&control.default) {
        edits.push(ControlEdit::Default(default));
    }
    let values = if draft.values.trim().is_empty() {
        None
    } else {
        Some(proposal_form::parse_enum_values(&draft.values)?)
    };
    if let Some(values) = values {
        if control.values.as_ref() != Some(&values) {
            edits.push(ControlEdit::Values(values));
        }
    } else if control.values.is_some() {
        return Err("Enter the menu choices; choices cannot be removed.".to_owned());
    }
    if draft.look != control.look_affecting {
        edits.push(ControlEdit::LookAffecting(draft.look));
    }
    let affects = proposal_form::parse_affects(&draft.affects)?;
    if affects != control.affects {
        edits.push(ControlEdit::Affects(affects));
    }
    if draft.spatial != control.spatial {
        edits.push(ControlEdit::Spatial(draft.spatial));
    }
    let tolerated = proposal_form::parse_optional_tolerated(&draft.tolerated)?;
    if let Some(tolerated) = tolerated {
        if control.tolerated.as_ref() != Some(&tolerated) {
            edits.push(ControlEdit::Tolerated(tolerated));
        }
    } else if control.tolerated.is_some() {
        return Err("Enter tolerated values, `any`, or leave them as published.".to_owned());
    }
    Ok(edits)
}

fn add_control(form: &Form) -> Result<ControlDefinition, String> {
    let kind = form.add_type.get();
    let numeric = matches!(kind, ControlType::Int | ControlType::Decimal);
    let range = numeric
        .then(|| {
            Ok::<_, String>(Range {
                min: proposal_form::parse_rational(&form.add_min.get())?,
                max: proposal_form::parse_rational(&form.add_max.get())?,
            })
        })
        .transpose()?;
    let step = numeric
        .then(|| proposal_form::parse_step(&form.add_step.get()))
        .transpose()?;
    let default = Some(proposal_form::parse_scalar(&form.add_default.get(), kind)?);
    let values = if kind == ControlType::Enum {
        let values = proposal_form::parse_enum_values(&form.add_values.get())?;
        if values.is_empty() {
            return Err("Enter at least one menu choice.".to_owned());
        }
        Some(values)
    } else {
        None
    };
    let tolerated = proposal_form::parse_optional_tolerated(&form.add_tolerated.get())?;
    let key = ControlKey::new(form.add_key.get().trim()).map_err(|error| error.to_string())?;
    Ok(proposal_form::control_definition(
        proposal_form::ControlDefinitionDraft {
            key,
            kind,
            range,
            step,
            default,
            values,
            look_affecting: form.add_look.get(),
            affects: proposal_form::parse_affects(&form.add_affects.get())?,
            spatial: form.add_spatial.get(),
            tolerated,
        },
    ))
}

fn describe_change(change: &ControlChange) -> String {
    match change {
        ControlChange::Add(definition) => {
            format!("adds control {} ({})", definition.key, definition.kind)
        }
        ControlChange::Set { edits, .. } => edits
            .iter()
            .map(describe_edit)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

fn describe_edit(edit: &ControlEdit) -> String {
    match edit {
        ControlEdit::Range(range) => format!("range {} to {}", range.min, range.max),
        ControlEdit::Step(step) => format!("step {step}"),
        ControlEdit::Default(value) => format!("default {}", proposal_form::scalar_text(value)),
        ControlEdit::Values(values) => format!(
            "choices {}",
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ControlEdit::LookAffecting(value) => format!("look affecting {value}"),
        ControlEdit::Affects(affects) => format!(
            "affects {}",
            affects
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ControlEdit::Spatial(value) => format!("spatial {value}"),
        ControlEdit::Tolerated(value) => {
            format!("tolerated {}", proposal_form::tolerated_text(value))
        }
    }
}

fn lut_dynamic_range_select(
    form: Form,
    id: &'static str,
    signal: RwSignal<Option<LutDynamicRange>>,
) -> impl IntoView {
    view! {
        <select id=id on:change=move |event| {
            form.manual.set(None);
            let value = event_target_value(&event);
            signal.set(if value.is_empty() { None } else { value.parse().ok() });
        }>
            <option value="" prop:selected=move || signal.get().is_none()>"Not declared"</option>
            <option value=LutDynamicRange::Dr100.to_string() prop:selected=move || signal.get() == Some(LutDynamicRange::Dr100)>"DR100"</option>
            <option value=LutDynamicRange::Dr200.to_string() prop:selected=move || signal.get() == Some(LutDynamicRange::Dr200)>"DR200"</option>
            <option value=LutDynamicRange::Dr400.to_string() prop:selected=move || signal.get() == Some(LutDynamicRange::Dr400)>"DR400"</option>
        </select>
    }
}

#[component]
fn LutFields(form: Form, index: SiteIndex) -> impl IntoView {
    let mut versions = proposal_form::base_look_versions(&index);
    if let Some(neutral) = proposal_form::neutral_base(&versions) {
        versions.insert(0, neutral);
    }
    let versions = StoredValue::new(versions);
    let published = StoredValue::new(index.looks);
    let bodies = StoredValue::new(index.bodies);
    view! {
        <section aria-labelledby="lut-heading">
            <h2 id="lut-heading">"The custom look"</h2>
            <p>"A custom look is a .cube LUT that changes how colors and brightness render. Published custom looks appear on Looks and in Compare; matchable ones also appear in Get the look."</p>
            <p><a href="https://github.com/skeet70/looklab/blob/main/docs/02-color-pipeline.md#38-lut-defined-looks">"How custom looks are built"</a></p>
            <crate::ui::DropZone on_files=Callback::new(move |selected| {
                let Some(file) = files::selected_files(selected).into_iter().next() else { return; };
                let generation = form.lut_generation.get_untracked().wrapping_add(1);
                form.lut_generation.set(generation);
                form.manual.set(None);
                form.lut_file.set(None);
                let name = file.name().to_owned();
                spawn_local(async move {
                    let bytes = match file.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            if form.lut_generation.try_get_untracked() == Some(generation) {
                                form.message.set(Some(error));
                            }
                            return;
                        }
                    };
                    if form.lut_generation.try_get_untracked() != Some(generation) {
                        return;
                    }
                    match proposal_form::parse_cube(name, bytes) {
                        Ok(parsed) => {
                            if form.lut_name.get().trim().is_empty() {
                                form.lut_name.set(parsed.summary.title.clone().unwrap_or_else(|| parsed.name.trim_end_matches(".cube").replace(['_', '-'], " ")));
                            }
                            form.lut_file.set(Some(parsed));
                        }
                        Err(error) => form.message.set(Some(error)),
                    }
                });
            }) accept=".cube".to_owned() folders=false/>
            {move || form.lut_file.get().map(|file| view! {
                <p class="muted">{format!("{} · {}³ nodes · domain {} to {} · sha256 {}", file.name, file.summary.size, file.summary.domain[0], file.summary.domain[1], &file.sha256.to_hex()[..16])}</p>
            })}
            <label for="lut-name">"Look name"</label>
            <input id="lut-name" prop:value=move || form.lut_name.get() on:input=move |event| { form.manual.set(None); form.lut_name.set(event_target_value(&event)); }/>
            <p class="muted">{move || looklab_schema::look_id_from_name(&form.lut_name.get()).map(|look| format!("Link: /looks/{look}")).unwrap_or_default()}</p>
            {move || {
                let look = looklab_schema::look_id_from_name(&form.lut_name.get()).ok()?;
                published.with_value(|looks| looks.iter().any(|entry| entry.slug.as_str() == look.as_str() && !entry.versions.is_empty())).then(|| view! {
                    <p class="error" role="alert">{format!("The link /looks/{look} already has published versions. Choose another name.")}</p>
                })
            }}
            <label for="lut-attribution">"Credit the author"</label>
            <input id="lut-attribution" prop:value=move || form.lut_attribution.get() on:input=move |event| { form.manual.set(None); form.lut_attribution.set(event_target_value(&event)); }/>
            <label class="consent"><input type="checkbox" prop:checked=move || form.lut_own_work.get() on:change=move |event| { form.manual.set(None); form.lut_own_work.set(event_target_checked(&event)); }/>"I made this look and may share it under the data license."</label>
            <label for="lut-source-license">"Or name the license that allows redistribution"</label>
            <input id="lut-source-license" placeholder="For example: CC0-1.0" prop:value=move || form.lut_source_license.get() on:input=move |event| { form.manual.set(None); form.lut_source_license.set(event_target_value(&event)); }/>
            <label for="lut-source-url">"Source URL"</label>
            <input id="lut-source-url" prop:value=move || form.lut_source_url.get() on:input=move |event| { form.manual.set(None); form.lut_source_url.set(event_target_value(&event)); }/>
            <label for="lut-encoding">"What the LUT expects"</label>
            <select id="lut-encoding" on:change=move |event| { form.manual.set(None); if let Ok(mode) = event_target_value(&event).parse::<LutEncodingMode>() { form.lut_encoding.set(mode); } }>
                <option value=LutEncodingMode::Scene.to_string() prop:selected=move || form.lut_encoding.get() == LutEncodingMode::Scene>"F-Log2 / F-Gamut (scene-referred)"</option>
                <option value=LutEncodingMode::Display.to_string() prop:selected=move || form.lut_encoding.get() == LutEncodingMode::Display>"Display-referred, from a base rendering"</option>
            </select>
            {move || (form.lut_encoding.get() == LutEncodingMode::Display).then(|| view! {
                <div>
                    <label for="lut-base-kind">"Base rendering"</label>
                    <select id="lut-base-kind" on:change=move |event| { form.manual.set(None); if let Ok(mode) = event_target_value(&event).parse::<LutBaseMode>() { form.lut_base_mode.set(mode); } }>
                        <option value=LutBaseMode::Look.to_string() prop:selected=move || form.lut_base_mode.get() == LutBaseMode::Look>"A published Look version"</option>
                        <option value=LutBaseMode::BodyRealization.to_string() prop:selected=move || form.lut_base_mode.get() == LutBaseMode::BodyRealization>"A camera or software rendering"</option>
                        <option value=LutBaseMode::FamilyRendering.to_string() prop:selected=move || form.lut_base_mode.get() == LutBaseMode::FamilyRendering>"A family rendering"</option>
                    </select>
                    {move || (form.lut_base_mode.get() == LutBaseMode::Look).then(|| view! {
                        <div>
                            <label for="lut-base-look">"Base Look version"</label>
                            <select id="lut-base-look" on:change=move |event| {
                                form.manual.set(None);
                                let Ok(index) = event_target_value(&event).parse::<usize>() else {
                                    form.lut_base.set(None);
                                    return;
                                };
                                versions.with_value(|versions| {
                                    form.lut_base.set(versions.get(index).cloned());
                                });
                            }>
                                <option value="">"Choose a version"</option>
                                {versions.with_value(|options| options.iter().enumerate().map(|(index, version)| {
                                    let label = if version.matchable {
                                        format!("{} · {}", version.name, version.version)
                                    } else {
                                        format!("{} · {} · preview base", version.name, version.version)
                                    };
                                    view! {
                                        <option value=index.to_string() prop:selected=move || form.lut_base.with(|selected| versions.with_value(|options| selected.as_ref() == options.get(index)))>{label}</option>
                                    }
                                }).collect_view())}
                            </select>
                            <p class="muted">"A published or rooted Look version, or the neutral baseline, is the base. Preview-only versions are labelled and are not matchable targets."</p>
                        </div>
                    })}
                    {move || (form.lut_base_mode.get() == LutBaseMode::BodyRealization).then(|| view! {
                        <div>
                            <label for="lut-base-body-mode">"Body source"</label>
                            <select id="lut-base-body-mode" on:change=move |event| {
                                form.manual.set(None);
                                if let Ok(mode) = event_target_value(&event).parse::<BodyPickerMode>() {
                                    form.lut_base_body_mode.set(mode);
                                    form.lut_base_body.set(None);
                                    form.lut_base_body_text.set(String::new());
                                }
                            }>
                                <option value=BodyPickerMode::Catalog.to_string() prop:selected=move || form.lut_base_body_mode.get() == BodyPickerMode::Catalog>"Catalog body"</option>
                                <option value=BodyPickerMode::Unregistered.to_string() prop:selected=move || form.lut_base_body_mode.get() == BodyPickerMode::Unregistered>"Unregistered body id"</option>
                            </select>
                            {move || (form.lut_base_body_mode.get() == BodyPickerMode::Catalog).then(|| view! {
                                <div>
                                    <label for="lut-base-body">"Camera body"</label>
                                    <select id="lut-base-body" on:change=move |event| {
                                        form.manual.set(None);
                                        let value = event_target_value(&event);
                                        form.lut_base_body.set(if value.is_empty() { None } else { BodyId::new(&value).ok() });
                                    }>
                                        <option value="" prop:selected=move || form.lut_base_body.get().is_none()>"Choose a body"</option>
                                        {bodies.with_value(|bodies| bodies.iter().map(|body| {
                                            let value = body.id.as_str().to_owned();
                                            let id = body.id.clone();
                                            let label = format!("{} {}", body.make, body.model);
                                            view! {
                                                <option value=value prop:selected=move || form.lut_base_body.get().as_ref() == Some(&id)>{label}</option>
                                            }
                                        }).collect_view())}
                                    </select>
                                </div>
                            })}
                            {move || (form.lut_base_body_mode.get() == BodyPickerMode::Unregistered).then(|| view! {
                                <div>
                                    <label for="lut-base-body-custom">"Body id"</label>
                                    <input id="lut-base-body-custom" placeholder="maker/model" prop:value=move || form.lut_base_body_text.get() on:input=move |event| {
                                        form.manual.set(None);
                                        let value = event_target_value(&event);
                                        form.lut_base_body.set(BodyId::new(value.trim()).ok());
                                        form.lut_base_body_text.set(value);
                                    }/>
                                    <p class="muted">"An unregistered body keeps the base incomplete until its schema is available."</p>
                                </div>
                            })}
                            <p class="muted">"Renderer: X RAW Studio"</p>
                            <label for="lut-base-sim">"Film simulation (optional)"</label>
                            <input id="lut-base-sim" placeholder="For example: classic_chrome" prop:value=move || form.lut_base_sim.get() on:input=move |event| { form.manual.set(None); form.lut_base_sim.set(event_target_value(&event)); }/>
                            <label for="lut-base-dr">"Dynamic range (optional)"</label>
                            {lut_dynamic_range_select(form, "lut-base-dr", form.lut_base_dynamic_range)}
                            <label for="lut-base-notes">"Notes (optional)"</label>
                            <input id="lut-base-notes" prop:value=move || form.lut_base_notes.get() on:input=move |event| { form.manual.set(None); form.lut_base_notes.set(event_target_value(&event)); }/>
                            <p class="muted">"The declaration is stored with its resolution status. A missing or unknown setting stays incomplete or inconsistent and previews on the neutral baseline; it is not matched."</p>
                        </div>
                    })}
                    {move || (form.lut_base_mode.get() == LutBaseMode::FamilyRendering).then(|| view! {
                        <div>
                            <label for="lut-family-maker">"Camera maker"</label>
                            <input id="lut-family-maker" prop:value=move || form.lut_family_maker.get() on:input=move |event| { form.manual.set(None); form.lut_family_maker.set(event_target_value(&event)); }/>
                            <label for="lut-family-rendering">"Rendering"</label>
                            <input id="lut-family-rendering" prop:value=move || form.lut_family_rendering.get() on:input=move |event| { form.manual.set(None); form.lut_family_rendering.set(event_target_value(&event)); }/>
                            <label for="lut-family-dr">"Dynamic range (optional)"</label>
                            {lut_dynamic_range_select(form, "lut-family-dr", form.lut_family_dynamic_range)}
                            <label for="lut-family-notes">"Notes (optional)"</label>
                            <input id="lut-family-notes" prop:value=move || form.lut_family_notes.get() on:input=move |event| { form.manual.set(None); form.lut_family_notes.set(event_target_value(&event)); }/>
                            <p class="muted">"A family rendering stays unmeasured until a native Look for that rendering is published."</p>
                        </div>
                    })}
                </div>
            })}
            <label for="lut-emulation">"What it reproduces"</label>
            <select id="lut-emulation" on:change=move |event| { form.manual.set(None); if let Ok(mode) = event_target_value(&event).parse::<LutEmulationMode>() { form.lut_emulation.set(mode); } }>
                <option value=LutEmulationMode::Original.to_string() prop:selected=move || form.lut_emulation.get() == LutEmulationMode::Original>"An original grade"</option>
                <option value=LutEmulationMode::CameraRendering.to_string() prop:selected=move || form.lut_emulation.get() == LutEmulationMode::CameraRendering>"A camera rendering"</option>
                <option value=LutEmulationMode::FilmStock.to_string() prop:selected=move || form.lut_emulation.get() == LutEmulationMode::FilmStock>"A film stock"</option>
            </select>
            {move || (form.lut_emulation.get() == LutEmulationMode::CameraRendering).then(|| view! {
                <div>
                    <label for="lut-emulation-maker">"Camera maker"</label>
                    <input id="lut-emulation-maker" prop:value=move || form.lut_emulation_maker.get() on:input=move |event| { form.manual.set(None); form.lut_emulation_maker.set(event_target_value(&event)); }/>
                    <label for="lut-emulation-rendering">"Camera rendering"</label>
                    <input id="lut-emulation-rendering" prop:value=move || form.lut_emulation_rendering.get() on:input=move |event| { form.manual.set(None); form.lut_emulation_rendering.set(event_target_value(&event)); }/>
                </div>
            })}
            {move || (form.lut_emulation.get() == LutEmulationMode::FilmStock).then(|| view! {
                <div>
                    <label for="lut-emulation-stock">"Film stock"</label>
                    <input id="lut-emulation-stock" prop:value=move || form.lut_emulation_stock.get() on:input=move |event| { form.manual.set(None); form.lut_emulation_stock.set(event_target_value(&event)); }/>
                </div>
            })}
        </section>
    }
}

#[component]
fn InterfaceFields(form: Form) -> impl IntoView {
    view! {
        <section aria-labelledby="interface-heading">
            <h2 id="interface-heading">"The editing-app profile"</h2>
            <p>"Choose a looklab editing-app profile (.yaml). The full file is uploaded with your proposal and stays private until approval."</p>
            <p><a href=crate::base::site_url("/developers/pipelines")>"About editing-app profiles"</a></p>
            <crate::ui::DropZone on_files=Callback::new(move |selected| {
                let Some(file) = files::selected_files(selected).into_iter().next() else { return; };
                let generation = form.interface_generation.get_untracked().wrapping_add(1);
                form.interface_generation.set(generation);
                form.manual.set(None);
                let name = file.name().to_owned();
                form.interface_text.set(String::new());
                form.interface_summary.set(None);
                form.interface_error.set(None);
                spawn_local(async move {
                    let bytes = match file.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            if form.interface_generation.try_get_untracked() == Some(generation) {
                                form.interface_error.set(Some(error));
                            }
                            return;
                        }
                    };
                    if form.interface_generation.try_get_untracked() != Some(generation) {
                        return;
                    }
                    match String::from_utf8(bytes) {
                        Ok(text) => match looklab_schema::validate_interface_profile(&text) {
                            Ok(profile) => {
                                form.interface_text.set(text);
                                form.interface_summary.set(Some((profile.id.to_string(), profile.name)));
                                form.interface_error.set(None);
                            }
                            Err(errors) => form.interface_error.set(Some(join_errors(&errors))),
                        },
                        Err(_) => form.interface_error.set(Some(format!("{name} is not a UTF-8 YAML file."))),
                    }
                });
            }) accept=".yaml,.yml".to_owned() folders=false/>
            {move || form.interface_error.get().map(|error| view! { <p class="error" role="alert">{error}</p> })}
            {move || form.interface_summary.get().map(|(id, name)| view! {
                <p class="muted">{format!("{name} · {id}")}</p>
            })}
        </section>
    }
}

#[component]
fn CameraSelect(
    cameras: Vec<looklab_publish::BodySummary>,
    selected: RwSignal<Option<BodyId>>,
    id: String,
    label: String,
    on_change: Callback<Option<BodyId>>,
) -> impl IntoView {
    let cameras = cameras
        .into_iter()
        .filter(|camera| camera.kind == looklab_publish::BodyKind::Camera)
        .collect::<Vec<_>>();
    view! {
        <label for=id.clone()>{label}</label>
        <select id=id on:change=move |event| {
            match BodyId::new(&event_target_value(&event)) {
                Ok(camera) => {
                    selected.set(Some(camera.clone()));
                    on_change.run(Some(camera));
                }
                Err(_) => {
                    selected.set(None);
                    on_change.run(None);
                }
            }
        }>
            <option value="">"Choose a camera"</option>
            {cameras.into_iter().map(|camera| {
                let choice = camera.id.clone();
                let value = camera.id.to_string();
                let label = format!("{} {}", camera.make, camera.model);
                view! {
                    <option value=value prop:selected=move || selected.get().as_ref() == Some(&choice)>
                        {label}
                    </option>
                }
            }).collect_view()}
        </select>
    }
}

#[component]
fn EvidenceFields(form: Form) -> impl IntoView {
    view! {
        {move || {
            let kind = form.kind.get();
            let relevant = kind == ProposalKind::Mapping
                || (kind == ProposalKind::Body
                    && form.body_kind.get() == looklab_schema::BodyKind::Camera);
            relevant.then(|| {
                let required = kind == ProposalKind::Body && form.body_mapping.get() == MappingStatus::FixtureBacked;
                view! {
                    <section aria-labelledby="evidence-heading">
                        <h2 id="evidence-heading">{if required { "Evidence required" } else { "Evidence (optional)" }}</h2>
                        <p>{if required {
                            "A camera confirmed from a sample file needs at least one original camera file donated under CC0. Only checked files leave this device."
                        } else {
                            "Without an original camera file, the mapping stays provisional. Only checked files leave this device."
                        }}</p>
                        {move || {
                            let (make, model) = form_evidence_identity(&form);
                            (!make.is_empty() && !model.is_empty()).then(|| view! {
                                <p class="muted">{format!("Donated files are recorded for {make} {model}.")}</p>
                            })
                        }}
                        <crate::ui::DropZone on_files=Callback::new(move |selected| {
                            let generation = form.fixture_generation.get_untracked().wrapping_add(1);
                            form.fixture_generation.set(generation);
                            form.pending.set(None);
                            form.fixture_files.set(Vec::new());
                            form.consented.set(Vec::new());
                            spawn_local(async move {
                                let read = files::selected_files(selected);
                                let mut parsed = Vec::with_capacity(read.len());
                                for file in read {
                                    let name = file.name().to_owned();
                                    let bytes = match file.bytes().await {
                                        Ok(bytes) => bytes,
                                        Err(error) => {
                                            if form.fixture_generation.try_get_untracked() == Some(generation) {
                                                form.message.set(Some(error));
                                            }
                                            return;
                                        }
                                    };
                                    if form.fixture_generation.try_get_untracked() != Some(generation) {
                                        return;
                                    }
                                    match FixtureFile::new(name, bytes) {
                                        Ok(file) => parsed.push(file),
                                        Err(error) => { form.message.set(Some(error)); return; }
                                    }
                                }
                                if form.fixture_generation.try_get_untracked() != Some(generation) {
                                    return;
                                }
                                form.consented.set(vec![false; parsed.len()]);
                                form.fixture_files.set(parsed);
                            });
                        }) accept=files::camera_originals_accept_attribute() folders=false/>
                        {move || form.fixture_files.get().into_iter().enumerate().map(|(index, file)| view! {
                            <label class="consent">
                                <input type="checkbox" id=format!("donate-{index}") prop:checked=move || form.consented.get().get(index).copied().unwrap_or(false) on:change=move |event| {
                                    let checked = event_target_checked(&event);
                                    form.pending.set(None);
                                    form.consented.update(|values| { if let Some(slot) = values.get_mut(index) { *slot = checked; } });
                                }/>
                                {format!("Donate {} under CC0-1.0 ({} bytes, sha256 {})", file.name, file.bytes.len(), &file.sha256.to_hex()[..16])}
                            </label>
                        }).collect_view()}
                    </section>
                }.into_any()
            })
        }}
    }
}

#[component]
fn Preview(form: Form) -> impl IntoView {
    view! {
        <section aria-labelledby="preview-heading">
            <h2 id="preview-heading">"Your proposed change"</h2>
            {move || {
                let text = match form.manual.get() {
                    Some(text) => text,
                    None => match computed_payload(&form) {
                        Ok(text) => text,
                        Err(error) => return view! { <p class="error" role="alert">{error}</p> }.into_any(),
                    },
                };
                match validate_proposal(form.kind.get(), &text) {
                    Ok(validated) => {
                        let summary = proposal_form::menu_summary(form.kind.get(), &validated.summary());
                        let details = summary.details.iter().map(|detail| view! { <li>{detail.clone()}</li> }).collect_view();
                        view! {
                            <div class="valid">
                                <p class="muted">"Ready for a moderator to review."</p>
                                <h3>{summary.headline}</h3>
                                <ul>{details}</ul>
                            </div>
                        }.into_any()
                    }
                    Err(errors) => {
                        let items = errors.iter().map(|error| view! { <li>{error.to_string()}</li> }).collect_view();
                        view! {
                            <div class="error" role="alert">
                                <strong>"Check these details before sending."</strong>
                                <ul>{items}</ul>
                            </div>
                        }.into_any()
                    }
                }
            }}
        </section>
    }
}

#[component]
fn Advanced(form: Form, payload: Signal<String>) -> impl IntoView {
    view! {
        <crate::ui::Disclosure summary="Advanced: the exact submission".to_owned()>
            <p class="muted">"Editing this replaces the form output until you reset it."</p>
            <label for="payload-yaml">"Payload (YAML, validated locally)"</label>
            <textarea id="payload-yaml" rows="16" cols="80" prop:value=move || payload.get() on:input=move |event| form.manual.set(Some(event_target_value(&event)))></textarea>
            <button type="button" on:click=move |_| form.manual.set(None)>"Use the form values"</button>
        </crate::ui::Disclosure>
    }
}
