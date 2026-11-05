//! Android's binary XML, read far enough to know what an exported APK's
//! `AndroidManifest.xml` declares: its package, version and SDK levels, and
//! the component its launcher intent starts.
//!
//! The file is a chunk stream: a string pool (UTF-16 or UTF-8), a resource
//! map naming the framework attribute each attribute-name string stands for,
//! and the element tree as start and end chunks. An attribute is identified by
//! its framework resource ID where the map gives one, as Android itself does,
//! and by its name otherwise. Godot 4.7.1's debug template declares its
//! launcher on an `activity-alias` (measured), which `aapt2 dump badging`
//! does not report, so the tree is read here rather than by a tool.
//!
//! The input was written by a process that ran project code: every count and
//! offset is checked against the file before it is used, and a string or a
//! chunk that would leave it is refused.

/// Why a manifest could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AxmlError {
    /// The bytes are not a binary XML document.
    #[error("the manifest is not Android binary XML")]
    NotBinaryXml,
    /// A chunk ends past the end of the document.
    #[error("the manifest is truncated: its {0} ends past the end of the document")]
    Truncated(&'static str),
    /// The string pool cannot be decoded.
    #[error("the manifest's string pool is malformed: {0}")]
    BadStrings(&'static str),
    /// An element or attribute names a string the pool does not have.
    #[error("the manifest names string {0}, which its string pool does not hold")]
    BadReference(u32),
    /// The document has no `<manifest package="...">` root.
    #[error("the manifest declares no package")]
    NoPackage,
}

/// `android:name`.
pub const ATTR_NAME: u32 = 0x0101_0003;
/// `android:enabled`.
pub const ATTR_ENABLED: u32 = 0x0101_000e;
/// `android:debuggable`.
pub const ATTR_DEBUGGABLE: u32 = 0x0101_000f;
/// `android:minSdkVersion`.
pub const ATTR_MIN_SDK: u32 = 0x0101_020c;
/// `android:targetActivity`.
pub const ATTR_TARGET_ACTIVITY: u32 = 0x0101_0202;
/// `android:versionCode`.
pub const ATTR_VERSION_CODE: u32 = 0x0101_021b;
/// `android:versionName`.
pub const ATTR_VERSION_NAME: u32 = 0x0101_021c;
/// `android:targetSdkVersion`.
pub const ATTR_TARGET_SDK: u32 = 0x0101_0270;

const CHUNK_STRING_POOL: u16 = 0x0001;
const CHUNK_XML: u16 = 0x0003;
const CHUNK_START_NAMESPACE: u16 = 0x0100;
const CHUNK_END_NAMESPACE: u16 = 0x0101;
const CHUNK_START_ELEMENT: u16 = 0x0102;
const CHUNK_END_ELEMENT: u16 = 0x0103;
const CHUNK_RESOURCE_MAP: u16 = 0x0180;
const NO_INDEX: u32 = u32::MAX;
const UTF8_FLAG: u32 = 1 << 8;
const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_BOOLEAN: u8 = 0x12;
/// The most strings a pool may hold.
const MAX_STRINGS: usize = 100_000;
/// The most elements a document may hold.
const MAX_ELEMENTS: usize = 100_000;

/// The action and category a launcher intent filter declares.
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";

/// One typed attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxmlValue {
    /// A string.
    String(String),
    /// An integer, decimal or hexadecimal.
    Int(i64),
    /// A boolean.
    Bool(bool),
    /// A resource reference, `@0x7f...`.
    Reference(u32),
    /// Another typed value, as its type and data.
    Other {
        /// The `Res_value` data type.
        data_type: u8,
        /// Its data.
        data: u32,
    },
}

/// One attribute of one element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    /// Its name string.
    pub name: String,
    /// Whether it is in a namespace (`android:` for the framework's).
    pub namespaced: bool,
    /// The framework attribute the resource map says it is.
    pub resource_id: Option<u32>,
    /// Its value.
    pub value: AxmlValue,
}

/// One start or end of an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An element starts.
    Start {
        /// Its name.
        name: String,
        /// Its attributes.
        attributes: Vec<Attribute>,
    },
    /// An element ends.
    End {
        /// Its name.
        name: String,
    },
}

fn u16_at(bytes: &[u8], at: usize, what: &'static str) -> Result<u16, AxmlError> {
    bytes
        .get(at..at + 2)
        .map(|field| u16::from_le_bytes([field[0], field[1]]))
        .ok_or(AxmlError::Truncated(what))
}

fn u32_at(bytes: &[u8], at: usize, what: &'static str) -> Result<u32, AxmlError> {
    bytes
        .get(at..at + 4)
        .map(|field| u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
        .ok_or(AxmlError::Truncated(what))
}

fn index(value: u32) -> Result<usize, AxmlError> {
    usize::try_from(value).map_err(|_| AxmlError::BadReference(value))
}

/// The strings of the pool chunk `chunk`.
fn string_pool(chunk: &[u8]) -> Result<Vec<String>, AxmlError> {
    let count = index(u32_at(chunk, 8, "string pool")?)?;
    let styles = index(u32_at(chunk, 12, "string pool")?)?;
    let flags = u32_at(chunk, 16, "string pool")?;
    let start = index(u32_at(chunk, 20, "string pool")?)?;
    let header = usize::from(u16_at(chunk, 2, "string pool")?);
    if count > MAX_STRINGS || styles > MAX_STRINGS {
        return Err(AxmlError::BadStrings("it holds too many strings"));
    }
    let utf8 = flags & UTF8_FLAG != 0;
    let mut strings = Vec::with_capacity(count);
    for slot in 0..count {
        let offset = index(u32_at(chunk, header + slot * 4, "string pool")?)?;
        let at = start
            .checked_add(offset)
            .ok_or(AxmlError::BadStrings("a string lies outside the pool"))?;
        strings.push(if utf8 {
            utf8_string(chunk, at)?
        } else {
            utf16_string(chunk, at)?
        });
    }
    Ok(strings)
}

fn utf16_string(chunk: &[u8], mut at: usize) -> Result<String, AxmlError> {
    let outside = |_| AxmlError::BadStrings("a string lies outside the pool");
    let mut length = usize::from(u16_at(chunk, at, "string pool").map_err(outside)?);
    at += 2;
    if length & 0x8000 != 0 {
        length = ((length & 0x7fff) << 16)
            | usize::from(u16_at(chunk, at, "string pool").map_err(outside)?);
        at += 2;
    }
    let units: Vec<u16> = chunk
        .get(at..at + length * 2)
        .ok_or(AxmlError::BadStrings("a string runs past the pool"))?
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| AxmlError::BadStrings("a string is not UTF-16"))
}

fn utf8_length(chunk: &[u8], at: &mut usize) -> Result<usize, AxmlError> {
    let first = *chunk
        .get(*at)
        .ok_or(AxmlError::BadStrings("a string runs past the pool"))?;
    *at += 1;
    if first & 0x80 == 0 {
        return Ok(usize::from(first));
    }
    let second = *chunk
        .get(*at)
        .ok_or(AxmlError::BadStrings("a string runs past the pool"))?;
    *at += 1;
    Ok((usize::from(first & 0x7f) << 8) | usize::from(second))
}

fn utf8_string(chunk: &[u8], mut at: usize) -> Result<String, AxmlError> {
    let _characters = utf8_length(chunk, &mut at)?;
    let length = utf8_length(chunk, &mut at)?;
    let bytes = chunk
        .get(at..at + length)
        .ok_or(AxmlError::BadStrings("a string runs past the pool"))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| AxmlError::BadStrings("a string is not UTF-8"))
}

fn string(strings: &[String], value: u32) -> Result<String, AxmlError> {
    strings
        .get(index(value)?)
        .cloned()
        .ok_or(AxmlError::BadReference(value))
}

/// Every element start and end of the binary XML document `bytes`, in order.
///
/// # Errors
/// Returns [`AxmlError`] for a document that is not binary XML, is
/// truncated, or names a string its pool does not hold.
pub fn events(bytes: &[u8]) -> Result<Vec<Event>, AxmlError> {
    if u16_at(bytes, 0, "header").ok() != Some(CHUNK_XML)
        || u16_at(bytes, 2, "header").ok() != Some(8)
    {
        return Err(AxmlError::NotBinaryXml);
    }
    let total = index(u32_at(bytes, 4, "header")?)?;
    let document = bytes.get(..total).ok_or(AxmlError::Truncated("document"))?;
    let mut strings: Vec<String> = Vec::new();
    let mut resources: Vec<u32> = Vec::new();
    let mut out = Vec::new();
    let mut at = 8;
    while at < total {
        let kind = u16_at(document, at, "chunk header")?;
        let header = usize::from(u16_at(document, at + 2, "chunk header")?);
        let size = index(u32_at(document, at + 4, "chunk header")?)?;
        if size < 8 || header < 8 || header > size {
            return Err(AxmlError::Truncated("chunk"));
        }
        let chunk = document
            .get(at..at + size)
            .ok_or(AxmlError::Truncated("chunk"))?;
        match kind {
            CHUNK_STRING_POOL => strings = string_pool(chunk)?,
            CHUNK_RESOURCE_MAP => {
                resources = chunk[header..]
                    .chunks_exact(4)
                    .map(|id| u32::from_le_bytes([id[0], id[1], id[2], id[3]]))
                    .collect();
            }
            CHUNK_START_ELEMENT => {
                if out.len() >= MAX_ELEMENTS * 2 {
                    return Err(AxmlError::Truncated("element tree"));
                }
                out.push(start_element(chunk, header, &strings, &resources)?);
            }
            CHUNK_END_ELEMENT => {
                let name = u32_at(chunk, header + 4, "element")?;
                out.push(Event::End {
                    name: string(&strings, name)?,
                });
            }
            CHUNK_START_NAMESPACE | CHUNK_END_NAMESPACE => {}
            _ => {}
        }
        at += size;
    }
    Ok(out)
}

fn start_element(
    chunk: &[u8],
    header: usize,
    strings: &[String],
    resources: &[u32],
) -> Result<Event, AxmlError> {
    let name = u32_at(chunk, header + 4, "element")?;
    let attribute_start = usize::from(u16_at(chunk, header + 8, "element")?);
    let attribute_size = usize::from(u16_at(chunk, header + 10, "element")?);
    let count = usize::from(u16_at(chunk, header + 12, "element")?);
    if attribute_size < 20 {
        return Err(AxmlError::Truncated("attribute"));
    }
    let mut attributes = Vec::with_capacity(count);
    for slot in 0..count {
        let at = header + attribute_start + slot * attribute_size;
        let namespace = u32_at(chunk, at, "attribute")?;
        let name_index = u32_at(chunk, at + 4, "attribute")?;
        let raw = u32_at(chunk, at + 8, "attribute")?;
        let data_type = *chunk
            .get(at + 15)
            .ok_or(AxmlError::Truncated("attribute"))?;
        let data = u32_at(chunk, at + 16, "attribute")?;
        let value = match data_type {
            TYPE_STRING => AxmlValue::String(string(strings, data)?),
            TYPE_INT_BOOLEAN => AxmlValue::Bool(data != 0),
            0x10..=0x11 => AxmlValue::Int(i64::from(i32::from_le_bytes(data.to_le_bytes()))),
            TYPE_REFERENCE => AxmlValue::Reference(data),
            _ if raw != NO_INDEX => AxmlValue::String(string(strings, raw)?),
            _ => AxmlValue::Other { data_type, data },
        };
        attributes.push(Attribute {
            name: string(strings, name_index)?,
            namespaced: namespace != NO_INDEX,
            resource_id: resources.get(index(name_index)?).copied(),
            value,
        });
    }
    Ok(Event::Start {
        name: string(strings, name)?,
        attributes,
    })
}

/// Which kind of component the launcher intent starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherKind {
    /// An `activity`.
    Activity,
    /// An `activity-alias` of another activity.
    ActivityAlias,
}

impl LauncherKind {
    /// The element name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::ActivityAlias => "activity-alias",
        }
    }
}

/// The component a manifest's launcher intent starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    /// What it is.
    pub kind: LauncherKind,
    /// Its fully qualified class name, which `am start -n` names.
    pub name: String,
    /// For an alias, the activity it stands for.
    pub target_activity: Option<String>,
}

/// What a manifest declares about its app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFacts {
    /// The application ID.
    pub package: String,
    /// `android:versionCode`.
    pub version_code: Option<i64>,
    /// `android:versionName`.
    pub version_name: Option<String>,
    /// `android:minSdkVersion`.
    pub min_sdk: Option<i64>,
    /// `android:targetSdkVersion`.
    pub target_sdk: Option<i64>,
    /// `android:debuggable` on the application.
    pub debuggable: Option<bool>,
    /// The first enabled component whose intent filter has the main action
    /// and the launcher category.
    pub launcher: Option<Launcher>,
}

fn attribute<'a>(attributes: &'a [Attribute], id: u32, name: &str) -> Option<&'a AxmlValue> {
    attributes
        .iter()
        .find(|attribute| {
            attribute.resource_id == Some(id)
                || (attribute.resource_id.is_none()
                    && attribute.namespaced
                    && attribute.name == name)
        })
        .map(|attribute| &attribute.value)
}

fn text(value: Option<&AxmlValue>) -> Option<String> {
    match value {
        Some(AxmlValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn integer(value: Option<&AxmlValue>) -> Option<i64> {
    match value {
        Some(AxmlValue::Int(number)) => Some(*number),
        Some(AxmlValue::String(text)) => text.parse().ok(),
        _ => None,
    }
}

fn boolean(value: Option<&AxmlValue>) -> Option<bool> {
    match value {
        Some(AxmlValue::Bool(flag)) => Some(*flag),
        Some(AxmlValue::String(text)) => text.parse().ok(),
        _ => None,
    }
}

/// A component's class name, qualified against the package the way Android
/// resolves `.Name` and `Name`.
fn qualify(package: &str, name: &str) -> String {
    if name.starts_with('.') {
        format!("{package}{name}")
    } else if name.contains('.') {
        name.to_owned()
    } else {
        format!("{package}.{name}")
    }
}

/// One activity or alias being read, and what its intent filters declared.
struct Component {
    kind: LauncherKind,
    name: Option<String>,
    target: Option<String>,
    enabled: bool,
    depth: usize,
    launcher: bool,
    filter: Option<(bool, bool)>,
}

/// Read what the binary manifest `bytes` declares.
///
/// # Errors
/// Returns [`AxmlError`] for a document that cannot be read, and
/// [`AxmlError::NoPackage`] for one without `<manifest package="...">`.
pub fn read_manifest(bytes: &[u8]) -> Result<ManifestFacts, AxmlError> {
    let events = events(bytes)?;
    let mut stack: Vec<&str> = Vec::new();
    let mut facts: Option<ManifestFacts> = None;
    let mut component: Option<Component> = None;
    for event in &events {
        match event {
            Event::Start { name, attributes } => {
                let parent = stack.last().copied();
                match (name.as_str(), parent) {
                    ("manifest", None) => {
                        let package = attributes
                            .iter()
                            .find(|attribute| !attribute.namespaced && attribute.name == "package")
                            .and_then(|attribute| text(Some(&attribute.value)))
                            .filter(|package| !package.is_empty())
                            .ok_or(AxmlError::NoPackage)?;
                        facts = Some(ManifestFacts {
                            package,
                            version_code: integer(attribute(
                                attributes,
                                ATTR_VERSION_CODE,
                                "versionCode",
                            )),
                            version_name: text(attribute(
                                attributes,
                                ATTR_VERSION_NAME,
                                "versionName",
                            )),
                            min_sdk: None,
                            target_sdk: None,
                            debuggable: None,
                            launcher: None,
                        });
                    }
                    ("uses-sdk", Some("manifest")) => {
                        if let Some(facts) = facts.as_mut() {
                            facts.min_sdk =
                                integer(attribute(attributes, ATTR_MIN_SDK, "minSdkVersion"));
                            facts.target_sdk =
                                integer(attribute(attributes, ATTR_TARGET_SDK, "targetSdkVersion"));
                        }
                    }
                    ("application", Some("manifest")) => {
                        if let Some(facts) = facts.as_mut() {
                            facts.debuggable =
                                boolean(attribute(attributes, ATTR_DEBUGGABLE, "debuggable"));
                        }
                    }
                    ("activity" | "activity-alias", Some("application")) => {
                        component = Some(Component {
                            kind: if name == "activity" {
                                LauncherKind::Activity
                            } else {
                                LauncherKind::ActivityAlias
                            },
                            name: text(attribute(attributes, ATTR_NAME, "name")),
                            target: text(attribute(
                                attributes,
                                ATTR_TARGET_ACTIVITY,
                                "targetActivity",
                            )),
                            enabled: boolean(attribute(attributes, ATTR_ENABLED, "enabled"))
                                .unwrap_or(true),
                            depth: stack.len(),
                            launcher: false,
                            filter: None,
                        });
                    }
                    ("intent-filter", Some("activity" | "activity-alias")) => {
                        if let Some(component) = component.as_mut() {
                            component.filter = Some((false, false));
                        }
                    }
                    ("action" | "category", Some("intent-filter")) => {
                        if let Some((main, launcher)) = component
                            .as_mut()
                            .and_then(|component| component.filter.as_mut())
                        {
                            let value = text(attribute(attributes, ATTR_NAME, "name"));
                            if name == "action" && value.as_deref() == Some(ACTION_MAIN) {
                                *main = true;
                            }
                            if name == "category" && value.as_deref() == Some(CATEGORY_LAUNCHER) {
                                *launcher = true;
                            }
                        }
                    }
                    _ => {}
                }
                stack.push(name.as_str());
            }
            Event::End { name } => {
                stack.pop();
                match name.as_str() {
                    "intent-filter" => {
                        if let Some(component) = component.as_mut()
                            && let Some((main, launcher)) = component.filter.take()
                        {
                            component.launcher |= main && launcher;
                        }
                    }
                    "activity" | "activity-alias" => {
                        if let Some(done) = component.take_if(|done| done.depth == stack.len())
                            && done.launcher
                            && done.enabled
                            && let (Some(facts), Some(class)) = (facts.as_mut(), done.name)
                            && facts.launcher.is_none()
                        {
                            let package = facts.package.clone();
                            facts.launcher = Some(Launcher {
                                kind: done.kind,
                                name: qualify(&package, &class),
                                target_activity: done
                                    .target
                                    .map(|target| qualify(&package, &target)),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    facts.ok_or(AxmlError::NoPackage)
}

/// Binary manifests laid out the way aapt2 and Godot write them, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub mod fixture {
    use super::{
        ATTR_DEBUGGABLE, ATTR_MIN_SDK, ATTR_NAME, ATTR_TARGET_ACTIVITY, ATTR_TARGET_SDK,
        ATTR_VERSION_CODE, ATTR_VERSION_NAME, NO_INDEX, TYPE_INT_BOOLEAN, TYPE_STRING, UTF8_FLAG,
    };

    /// The component a fixture manifest's launcher intent starts.
    #[derive(Debug, Clone)]
    pub enum Launcher {
        /// No launcher intent filter.
        None,
        /// An activity with the filter.
        Activity(String),
        /// An alias with the filter, of the activity it names.
        Alias {
            /// The alias's class name.
            name: String,
            /// The activity it stands for.
            target: String,
        },
    }

    /// What a fixture manifest declares.
    #[derive(Debug, Clone)]
    pub struct Spec {
        /// The application ID.
        pub package: String,
        /// `android:versionCode`.
        pub version_code: i32,
        /// `android:versionName`.
        pub version_name: String,
        /// `android:minSdkVersion`.
        pub min_sdk: i32,
        /// `android:targetSdkVersion`.
        pub target_sdk: i32,
        /// `android:debuggable` on the application.
        pub debuggable: bool,
        /// The launcher component.
        pub launcher: Launcher,
        /// Whether the string pool is UTF-8 rather than Godot's UTF-16.
        pub utf8: bool,
    }

    impl Spec {
        /// Godot 4.7.1's debug manifest shape: the launcher on an alias.
        #[must_use]
        pub fn godot(package: &str) -> Self {
            Self {
                package: package.to_owned(),
                version_code: 1,
                version_name: "1.0.0".to_owned(),
                min_sdk: 24,
                target_sdk: 36,
                debuggable: true,
                launcher: Launcher::Alias {
                    name: "com.godot.game.GodotAppLauncher".to_owned(),
                    target: "com.godot.game.GodotApp".to_owned(),
                },
                utf8: false,
            }
        }
    }

    enum Value {
        String(u32),
        Int(i32),
        Bool(bool),
    }

    struct Writer {
        strings: Vec<String>,
        body: Vec<u8>,
    }

    impl Writer {
        fn string(&mut self, text: &str) -> u32 {
            if let Some(found) = self.strings.iter().position(|known| known == text) {
                return u32::try_from(found).expect("index");
            }
            self.strings.push(text.to_owned());
            u32::try_from(self.strings.len() - 1).expect("index")
        }

        fn start(&mut self, name: &str, attributes: &[(u32, Option<u32>, Value)]) {
            let name = self.string(name);
            let mut chunk = Vec::new();
            chunk.extend_from_slice(&0x0102_u16.to_le_bytes());
            chunk.extend_from_slice(&16_u16.to_le_bytes());
            let size = 16 + 20 + 20 * attributes.len();
            chunk.extend_from_slice(&u32::try_from(size).expect("size").to_le_bytes());
            chunk.extend_from_slice(&1_u32.to_le_bytes());
            chunk.extend_from_slice(&NO_INDEX.to_le_bytes());
            chunk.extend_from_slice(&NO_INDEX.to_le_bytes());
            chunk.extend_from_slice(&name.to_le_bytes());
            chunk.extend_from_slice(&20_u16.to_le_bytes());
            chunk.extend_from_slice(&20_u16.to_le_bytes());
            chunk.extend_from_slice(
                &u16::try_from(attributes.len())
                    .expect("count")
                    .to_le_bytes(),
            );
            chunk.extend_from_slice(&[0; 6]);
            for (name, namespace, value) in attributes {
                chunk.extend_from_slice(&namespace.unwrap_or(NO_INDEX).to_le_bytes());
                chunk.extend_from_slice(&name.to_le_bytes());
                let (raw, data_type, data) = match value {
                    Value::String(index) => (*index, TYPE_STRING, *index),
                    Value::Int(number) => {
                        (NO_INDEX, 0x10, u32::from_le_bytes(number.to_le_bytes()))
                    }
                    Value::Bool(flag) => {
                        (NO_INDEX, TYPE_INT_BOOLEAN, if *flag { u32::MAX } else { 0 })
                    }
                };
                chunk.extend_from_slice(&raw.to_le_bytes());
                chunk.extend_from_slice(&8_u16.to_le_bytes());
                chunk.push(0);
                chunk.push(data_type);
                chunk.extend_from_slice(&data.to_le_bytes());
            }
            self.body.extend_from_slice(&chunk);
        }

        fn end(&mut self, name: &str) {
            let name = self.string(name);
            self.body.extend_from_slice(&0x0103_u16.to_le_bytes());
            self.body.extend_from_slice(&16_u16.to_le_bytes());
            self.body.extend_from_slice(&24_u32.to_le_bytes());
            self.body.extend_from_slice(&1_u32.to_le_bytes());
            self.body.extend_from_slice(&NO_INDEX.to_le_bytes());
            self.body.extend_from_slice(&NO_INDEX.to_le_bytes());
            self.body.extend_from_slice(&name.to_le_bytes());
        }

        fn pool(&self, utf8: bool) -> Vec<u8> {
            let mut data = Vec::new();
            let mut offsets = Vec::new();
            for text in &self.strings {
                offsets.push(u32::try_from(data.len()).expect("offset"));
                if utf8 {
                    let characters = text.chars().count();
                    for length in [characters, text.len()] {
                        if length > 0x7f {
                            data.push(u8::try_from(0x80 | (length >> 8)).expect("long length"));
                            data.push(u8::try_from(length & 0xff).expect("long length"));
                        } else {
                            data.push(u8::try_from(length).expect("length"));
                        }
                    }
                    data.extend_from_slice(text.as_bytes());
                    data.push(0);
                } else {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    data.extend_from_slice(
                        &u16::try_from(units.len()).expect("length").to_le_bytes(),
                    );
                    for unit in units {
                        data.extend_from_slice(&unit.to_le_bytes());
                    }
                    data.extend_from_slice(&[0, 0]);
                }
            }
            data.resize(data.len().div_ceil(4) * 4, 0);
            let start = 28 + 4 * self.strings.len();
            let mut chunk = Vec::new();
            chunk.extend_from_slice(&0x0001_u16.to_le_bytes());
            chunk.extend_from_slice(&28_u16.to_le_bytes());
            chunk.extend_from_slice(
                &u32::try_from(start + data.len())
                    .expect("size")
                    .to_le_bytes(),
            );
            chunk.extend_from_slice(
                &u32::try_from(self.strings.len())
                    .expect("count")
                    .to_le_bytes(),
            );
            chunk.extend_from_slice(&0_u32.to_le_bytes());
            chunk.extend_from_slice(&(if utf8 { UTF8_FLAG } else { 0 }).to_le_bytes());
            chunk.extend_from_slice(&u32::try_from(start).expect("start").to_le_bytes());
            chunk.extend_from_slice(&0_u32.to_le_bytes());
            for offset in offsets {
                chunk.extend_from_slice(&offset.to_le_bytes());
            }
            chunk.extend_from_slice(&data);
            chunk
        }
    }

    /// The binary `AndroidManifest.xml` of `spec`.
    #[must_use]
    pub fn manifest(spec: &Spec) -> Vec<u8> {
        // The framework attributes come first in the pool, so the resource
        // map names them by index, as aapt2 lays them out.
        let framework = [
            ("name", ATTR_NAME),
            ("debuggable", ATTR_DEBUGGABLE),
            ("minSdkVersion", ATTR_MIN_SDK),
            ("targetActivity", ATTR_TARGET_ACTIVITY),
            ("versionCode", ATTR_VERSION_CODE),
            ("versionName", ATTR_VERSION_NAME),
            ("targetSdkVersion", ATTR_TARGET_SDK),
        ];
        let mut writer = Writer {
            strings: framework
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            body: Vec::new(),
        };
        let id = |name: &str| {
            u32::try_from(
                framework
                    .iter()
                    .position(|(known, _)| *known == name)
                    .expect("a framework attribute"),
            )
            .expect("index")
        };
        let android = writer.string("http://schemas.android.com/apk/res/android");
        let package_attribute = writer.string("package");
        let package = writer.string(&spec.package);
        let version_name = writer.string(&spec.version_name);
        writer.start(
            "manifest",
            &[
                (
                    id("versionCode"),
                    Some(android),
                    Value::Int(spec.version_code),
                ),
                (
                    id("versionName"),
                    Some(android),
                    Value::String(version_name),
                ),
                (package_attribute, None, Value::String(package)),
            ],
        );
        writer.start(
            "uses-sdk",
            &[
                (id("minSdkVersion"), Some(android), Value::Int(spec.min_sdk)),
                (
                    id("targetSdkVersion"),
                    Some(android),
                    Value::Int(spec.target_sdk),
                ),
            ],
        );
        writer.end("uses-sdk");
        writer.start(
            "application",
            &[(
                id("debuggable"),
                Some(android),
                Value::Bool(spec.debuggable),
            )],
        );
        let filter = |writer: &mut Writer| {
            let main = writer.string("android.intent.action.MAIN");
            let launcher = writer.string("android.intent.category.LAUNCHER");
            let default = writer.string("android.intent.category.DEFAULT");
            writer.start("intent-filter", &[]);
            writer.start(
                "action",
                &[(id("name"), Some(android), Value::String(main))],
            );
            writer.end("action");
            writer.start(
                "category",
                &[(id("name"), Some(android), Value::String(default))],
            );
            writer.end("category");
            writer.start(
                "category",
                &[(id("name"), Some(android), Value::String(launcher))],
            );
            writer.end("category");
            writer.end("intent-filter");
        };
        match &spec.launcher {
            Launcher::None => {
                let name = writer.string("com.godot.game.GodotApp");
                writer.start(
                    "activity",
                    &[(id("name"), Some(android), Value::String(name))],
                );
                writer.end("activity");
            }
            Launcher::Activity(class) => {
                let name = writer.string(class);
                writer.start(
                    "activity",
                    &[(id("name"), Some(android), Value::String(name))],
                );
                filter(&mut writer);
                writer.end("activity");
            }
            Launcher::Alias { name, target } => {
                let target_name = writer.string(target);
                writer.start(
                    "activity",
                    &[(id("name"), Some(android), Value::String(target_name))],
                );
                writer.end("activity");
                let alias = writer.string(name);
                writer.start(
                    "activity-alias",
                    &[
                        (id("name"), Some(android), Value::String(alias)),
                        (
                            id("targetActivity"),
                            Some(android),
                            Value::String(target_name),
                        ),
                    ],
                );
                filter(&mut writer);
                writer.end("activity-alias");
            }
        }
        writer.end("application");
        writer.end("manifest");
        let pool = writer.pool(spec.utf8);
        let mut map = Vec::new();
        map.extend_from_slice(&0x0180_u16.to_le_bytes());
        map.extend_from_slice(&8_u16.to_le_bytes());
        map.extend_from_slice(
            &u32::try_from(8 + 4 * framework.len())
                .expect("size")
                .to_le_bytes(),
        );
        for (_, resource) in framework {
            map.extend_from_slice(&resource.to_le_bytes());
        }
        let size = 8 + pool.len() + map.len() + writer.body.len();
        let mut out = Vec::new();
        out.extend_from_slice(&0x0003_u16.to_le_bytes());
        out.extend_from_slice(&8_u16.to_le_bytes());
        out.extend_from_slice(&u32::try_from(size).expect("size").to_le_bytes());
        out.extend_from_slice(&pool);
        out.extend_from_slice(&map);
        out.extend_from_slice(&writer.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Launcher as Declared, Spec, manifest};
    use super::*;

    #[test]
    fn a_godot_manifest_names_its_package_and_its_launcher_alias() {
        for utf8 in [false, true] {
            let spec = Spec {
                utf8,
                ..Spec::godot("org.godotmcp.measure")
            };
            let facts = read_manifest(&manifest(&spec)).expect("a Godot-shaped manifest");
            assert_eq!(facts.package, "org.godotmcp.measure");
            assert_eq!(facts.version_code, Some(1));
            assert_eq!(facts.version_name.as_deref(), Some("1.0.0"));
            assert_eq!(facts.min_sdk, Some(24));
            assert_eq!(facts.target_sdk, Some(36));
            assert_eq!(facts.debuggable, Some(true));
            assert_eq!(
                facts.launcher,
                Some(Launcher {
                    kind: LauncherKind::ActivityAlias,
                    name: "com.godot.game.GodotAppLauncher".to_owned(),
                    target_activity: Some("com.godot.game.GodotApp".to_owned()),
                }),
                "utf8={utf8}"
            );
        }
    }

    #[test]
    fn a_relative_activity_name_is_qualified_by_the_package() {
        let spec = Spec {
            launcher: Declared::Activity(".Main".to_owned()),
            ..Spec::godot("org.example.game")
        };
        let launcher = read_manifest(&manifest(&spec))
            .expect("manifest")
            .launcher
            .expect("a launcher");
        assert_eq!(launcher.kind, LauncherKind::Activity);
        assert_eq!(launcher.name, "org.example.game.Main");
        assert_eq!(qualify("org.example", "Main"), "org.example.Main");
        assert_eq!(qualify("org.example", "com.other.Main"), "com.other.Main");
    }

    #[test]
    fn a_manifest_without_a_launcher_says_so() {
        let spec = Spec {
            launcher: Declared::None,
            ..Spec::godot("org.example.game")
        };
        assert_eq!(
            read_manifest(&manifest(&spec)).expect("manifest").launcher,
            None
        );
    }

    #[test]
    fn what_is_not_a_readable_manifest_is_refused() {
        assert_eq!(read_manifest(b""), Err(AxmlError::NotBinaryXml));
        assert_eq!(
            read_manifest(b"<?xml version=\"1.0\"?><manifest/>"),
            Err(AxmlError::NotBinaryXml)
        );
        let bytes = manifest(&Spec::godot("org.example.game"));
        for length in [8, 20, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                read_manifest(&bytes[..length]).is_err(),
                "a manifest cut at {length} bytes"
            );
        }
        // A string offset that leaves the pool.
        let mut damaged = bytes.clone();
        damaged[8 + 28..8 + 32].copy_from_slice(&0x00ff_ffff_u32.to_le_bytes());
        assert!(matches!(
            read_manifest(&damaged),
            Err(AxmlError::BadStrings(_))
        ));
    }
}
