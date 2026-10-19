//! Language-neutral GLSL ES source adaptation for WebGL host backends.
//!
//! This only handles the ES-to-core dialect bridge. The actual GLSL grammar,
//! validation, and shader compilation remain the responsibility of the host's
//! pinned shader compiler.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Vertex,
    Fragment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniformDeclaration {
    pub name: String,
    pub ty: String,
    pub array_len: Option<usize>,
}

pub fn uniform_declarations(source: &str) -> Vec<UniformDeclaration> {
    let mut declarations = Vec::new();
    for line in source.lines() {
        let source = line.split_once("//").map_or(line, |(code, _)| code);
        for statement in source.split(';') {
            let statement = statement.trim();
            let Some(declaration) = statement.strip_prefix("uniform ") else {
                continue;
            };
            let mut words = declaration.split_whitespace();
            let (Some(ty), Some(name)) = (words.next(), words.next()) else {
                continue;
            };
            if words.next().is_some() || ty.starts_with('{') {
                continue;
            }
            let (name, array_len) = if let Some((name, length)) = name.split_once('[') {
                let Some(length) = length.strip_suffix(']') else {
                    continue;
                };
                let Ok(length) = length.parse::<usize>() else {
                    continue;
                };
                if length == 0 || length > 256 {
                    continue;
                }
                (name, Some(length))
            } else {
                (name, None)
            };
            let name = name.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_'
            });
            if !name.is_empty()
                && !declarations
                    .iter()
                    .any(|entry: &UniformDeclaration| entry.name == name)
            {
                declarations.push(UniformDeclaration {
                    name: name.to_owned(),
                    ty: ty.to_owned(),
                    array_len,
                });
            }
        }
    }
    declarations
}

/// Finds vertex input names used to implement the WebGL attribute-location API.
/// Full GLSL parsing and validation still happens in the backend compiler.
pub fn vertex_attribute_names(source: &str, stage: Stage) -> Vec<String> {
    if stage != Stage::Vertex {
        return Vec::new();
    }
    source
        .lines()
        .flat_map(|line| {
            line.split_once("//")
                .map_or(line, |(code, _)| code)
                .split(';')
                .map(|statement| statement.trim().to_owned())
                .collect::<Vec<_>>()
        })
        .filter_map(|declaration| {
            let words = declaration.split_whitespace().collect::<Vec<_>>();
            let qualifier = words
                .iter()
                .position(|word| *word == "attribute" || *word == "in")?;
            if words.get(qualifier + 1).is_none() {
                return None;
            }
            let name = words.last()?.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_'
            });
            (!name.is_empty()).then(|| name.to_owned())
        })
        .take(16)
        .collect()
}

/// Adapts common GLSL ES 1.00/3.00 declarations to GLSL 4.50 for a host
/// compiler. Unsupported directives are rejected explicitly instead of being
/// silently discarded.
pub fn translate_es_to_glsl_450(source: &str, stage: Stage) -> Result<String, String> {
    translate_es_to_glsl_450_with_uniforms(source, stage, &HashMap::new())
}

pub fn translate_es_to_glsl_450_with_uniforms(
    source: &str,
    stage: Stage,
    uniforms: &HashMap<String, Vec<f64>>,
) -> Result<String, String> {
    let mut body = String::new();
    let mut input_location = 0u32;
    let mut varying_location = 0u32;
    let mut output_location = 0u32;
    let mut fragment_color = false;
    let mut samplers = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#version") {
            continue;
        }
        if trimmed.starts_with("#extension") {
            return Err("GLSL extensions are not supported by this WebGL context".into());
        }
        let translated = line
            .replace("highp ", "")
            .replace("mediump ", "")
            .replace("lowp ", "")
            .replace("texture2D(", "texture(")
            .replace("textureCube(", "texture(")
            // GLSL ES spells the vertex and instance indices differently from
            // Vulkan GLSL, which is the target accepted by the pinned Naga
            // compiler. These are equivalent for WebGL's zero-based draws.
            .replace("gl_VertexID", "gl_VertexIndex")
            .replace("gl_InstanceID", "gl_InstanceIndex");
        let terminated = translated.trim_end().ends_with(';');
        let parts = translated.split(';').collect::<Vec<_>>();
        for (index, part) in parts.iter().enumerate() {
            let leading = part.len() - part.trim_start().len();
            let content = &part[leading..];
            if content.trim().is_empty() {
                continue;
            }
            if content.trim_start().starts_with("precision ") {
                continue;
            }
            if let Some(rest) = content.trim_start().strip_prefix("uniform ") {
                let declaration = rest.trim();
                let mut words = declaration.split_whitespace();
                let (Some(ty), Some(name)) = (words.next(), words.next()) else {
                    return Err("unsupported GLSL uniform declaration".into());
                };
                if words.next().is_some() {
                    return Err("uniform structures are not supported by this backend".into());
                }
                let (name, array_len) = if let Some((name, length)) = name.split_once('[') {
                    let length = length
                        .strip_suffix(']')
                        .ok_or_else(|| "invalid uniform array declaration".to_owned())?;
                    let length = length
                        .parse::<usize>()
                        .map_err(|_| "invalid uniform array length".to_owned())?;
                    if length == 0 || length > 256 {
                        return Err("uniform array length is outside the supported range".into());
                    }
                    (name, Some(length))
                } else {
                    (name, None)
                };
                let name = name.trim_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && character != '_'
                });
                if ty == "sampler2D" {
                    if array_len.is_some() {
                        return Err("sampler arrays are not supported by this backend".into());
                    }
                    let binding = samplers.len() as u32 * 2;
                    body.push_str(&format!(
                        "layout(set = 0, binding = {binding}) uniform texture2D {name};"
                    ));
                    body.push_str(&format!(
                        "layout(set = 0, binding = {}) uniform sampler {name}_sampler;",
                        binding + 1
                    ));
                    samplers.push(name.to_owned());
                    continue;
                }
                let components = match ty {
                    "float" | "int" | "uint" | "bool" => 1,
                    "vec2" | "ivec2" | "uvec2" | "bvec2" => 2,
                    "vec3" | "ivec3" | "uvec3" | "bvec3" => 3,
                    "vec4" | "ivec4" | "uvec4" | "bvec4" => 4,
                    "mat2" => 4,
                    "mat3" => 9,
                    "mat4" => 16,
                    _ => return Err(format!("unsupported WebGL uniform type: {ty}")),
                };
                let values = uniforms.get(name);
                let construct = |components: usize, offset: usize| {
                    if ty.starts_with("mat") {
                        let values = (0..components)
                            .map(|index| {
                                format_uniform_component(
                                    "float",
                                    values
                                        .and_then(|values| values.get(offset + index))
                                        .copied()
                                        .unwrap_or(0.0),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{ty}({values})")
                    } else if components == 1 {
                        format_uniform_component(
                            ty,
                            values
                                .and_then(|values| values.get(offset))
                                .copied()
                                .unwrap_or(0.0),
                        )
                    } else {
                        let vector = ty.strip_suffix(&components.to_string()).unwrap_or("vec");
                        let values = (0..components)
                            .map(|index| {
                                format_uniform_component(
                                    ty,
                                    values
                                        .and_then(|values| values.get(offset + index))
                                        .copied()
                                        .unwrap_or(0.0),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{vector}{components}({values})")
                    }
                };
                let literal = if let Some(array_len) = array_len {
                    let mut elements = Vec::with_capacity(array_len);
                    for index in 0..array_len {
                        elements.push(construct(components, index * components));
                    }
                    format!("{ty}[{array_len}]({})", elements.join(", "))
                } else {
                    construct(components, 0)
                };
                if let Some(array_len) = array_len {
                    body.push_str(&format!("const {ty} {name}[{array_len}] = {literal};"));
                } else {
                    body.push_str(&format!("const {ty} {name} = {literal};"));
                }
                continue;
            }
            let content = content.trim_start();
            let (storage, declaration) = if let Some(rest) = content.strip_prefix("attribute ") {
                (Some("in"), rest)
            } else if let Some(rest) = content.strip_prefix("varying ") {
                (
                    Some(if stage == Stage::Vertex { "out" } else { "in" }),
                    rest,
                )
            } else if let Some(rest) = content.strip_prefix("in ") {
                (Some("in"), rest)
            } else if let Some(rest) = content.strip_prefix("out ") {
                (Some("out"), rest)
            } else {
                (None, content)
            };
            if let Some(storage) = storage {
                let location = match (stage, storage) {
                    (Stage::Vertex, "in") => &mut input_location,
                    (Stage::Fragment, "out") => &mut output_location,
                    _ => &mut varying_location,
                };
                let qualifier_location = format!("layout(location = {}) {storage} ", *location);
                body.push_str(&format!("{qualifier_location}{declaration}"));
                *location = location.saturating_add(1);
            } else {
                let mut statement = format!("{}{}", &part[..leading], content);
                if stage == Stage::Fragment && statement.contains("gl_FragColor") {
                    fragment_color = true;
                    statement = statement.replace("gl_FragColor", "lumen_frag_color");
                }
                body.push_str(&statement);
            }
            if index + 1 < parts.len() || terminated {
                body.push(';');
            }
        }
        body.push('\n');
    }
    for sampler in samplers {
        body = body.replace(
            &format!("texture({sampler},"),
            &format!("texture(sampler2D({sampler}, {sampler}_sampler),"),
        );
        body = body.replace(
            &format!("texture ({sampler},"),
            &format!("texture(sampler2D({sampler}, {sampler}_sampler),"),
        );
    }
    let mut output = String::from("#version 450\n");
    if fragment_color {
        output.push_str("layout(location = 0) out vec4 lumen_frag_color;\n");
    }
    output.push_str(&body);
    Ok(output)
}

fn format_uniform_component(ty: &str, value: f64) -> String {
    if ty.starts_with('i') {
        format!("{}", value as i32)
    } else if ty.starts_with('u') {
        format!("{}u", (value.max(0.0)) as u32)
    } else if ty.starts_with('b') {
        if value == 0.0 {
            "false".into()
        } else {
            "true".into()
        }
    } else if value.is_finite() {
        let mut value = value.to_string();
        if !value.contains('.') && !value.contains('e') && !value.contains('E') {
            value.push_str(".0");
        }
        value
    } else {
        "0.0".into()
    }
}
