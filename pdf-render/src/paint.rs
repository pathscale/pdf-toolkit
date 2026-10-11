use alloc::sync::Arc;
use alloc::vec::Vec;

use pdf_core::{DictRef, DocError, LimitKind, Resolver, Value, ValueRef};
use pdf_text::content::{decode_name, decode_string, Operand};

use crate::display::{
    bad, check, checked_point, finite, number, read_array, resource, refusal,
    warn, Matrix, Paint, Rgba,
};

const COMPONENTS: usize = 32;
const SPACE_DEPTH: u64 = 16;
const SAMPLES: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Basic {
    Gray,
    Rgb,
    Cmyk,
    Unknown,
}

impl Basic {
    fn components(self) -> usize {
        match self {
            Self::Gray | Self::Unknown => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
        }
    }

    fn color(self, values: &[f32; COMPONENTS], alpha: f32) -> Rgba {
        let component = |index| unit(values.get(index).copied().unwrap_or(0.0));
        let (r, g, b) = match self {
            Self::Gray => {
                let gray = component(0);
                (gray, gray, gray)
            }
            Self::Rgb => (component(0), component(1), component(2)),
            Self::Cmyk => {
                let black = 1.0 - component(3);
                ((1.0 - component(0)) * black,
                 (1.0 - component(1)) * black,
                 (1.0 - component(2)) * black)
            }
            Self::Unknown => (0.5, 0.5, 0.5),
        };
        Rgba { r, g, b, a: unit(alpha) }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Mapping {
    Indexed { palette: Arc<[u8]>, hival: u8, outputs: usize },
    Tint { function: Option<Type2>, inputs: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ColorSpace {
    basic: Basic,
    mappings: Vec<Mapping>,
    components: usize,
    pattern: bool,
}

impl ColorSpace {
    fn basic(basic: Basic) -> Arc<Self> {
        Arc::new(Self {
            basic, mappings: Vec::new(), components: basic.components(), pattern: false,
        })
    }

    pub(crate) fn device_gray() -> Arc<Self> { Self::basic(Basic::Gray) }
    pub(crate) fn device_rgb() -> Arc<Self> { Self::basic(Basic::Rgb) }
    pub(crate) fn device_cmyk() -> Arc<Self> { Self::basic(Basic::Cmyk) }

    pub(crate) fn named_or_resource(
        resolver: &mut Resolver<'_>,
        resources: Option<&Value>,
        name: &[u8],
    ) -> Result<Arc<Self>, DocError> {
        if name == b"Pattern" {
            return Ok(Arc::new(Self {
                basic: Basic::Unknown, mappings: Vec::new(), components: 0, pattern: true,
            }));
        }
        if let Some(basic) = named_basic(name) { return Ok(Self::basic(basic)); }
        if let Some(value) = resource(resolver, resources, b"ColorSpace", name)? {
            return Self::load(resolver, resources, value.get());
        }
        warn(resolver, None, "render_colorspace_unsupported");
        Ok(Self::basic(Basic::Unknown))
    }

    fn load(
        resolver: &mut Resolver<'_>,
        resources: Option<&Value>,
        value: ValueRef<'_>,
    ) -> Result<Arc<Self>, DocError> {
        let mut current = resolver.resolve(value)?;
        let mut mappings = Vec::new();
        let mut pattern = false;
        let mut seen = Vec::new();
        let mut basic = Basic::Unknown;
        for depth in 0..=SPACE_DEPTH {
            check(LimitKind::Depth, depth.saturating_add(1), SPACE_DEPTH)?;
            if let Some(name) = current.get().as_name() {
                let name = name.decoded();
                if let Some(found) = named_basic(name.as_ref()) {
                    basic = found;
                    break;
                }
                if name.as_ref() == b"Pattern" {
                    pattern = true;
                    break;
                }
                if let Some(value) = resource(resolver, resources, b"ColorSpace", name.as_ref())? {
                    let identity = (value.object().buffer().as_ptr() as usize, value.get().span());
                    if seen.contains(&identity) { return Err(bad("colorspace_cycle")); }
                    bounded_push(resolver, &mut seen, identity)?;
                    current = value;
                    continue;
                }
                warn(resolver, current.object().id(), "render_colorspace_unsupported");
                break;
            }
            let Some(array) = current.get().as_array() else {
                warn(resolver, current.object().id(), "render_colorspace_unsupported");
                break;
            };
            let first = resolver.resolve(array.get(0).ok_or_else(|| bad("colorspace_array"))?)?;
            let Some(name) = first.get().as_name() else { return Err(bad("colorspace_name")) };
            if name.eq_bytes(b"CalGray") {
                basic = Basic::Gray;
                break;
            }
            if name.eq_bytes(b"CalRGB") {
                basic = Basic::Rgb;
                break;
            }
            if name.eq_bytes(b"ICCBased") {
                let profile = resolver.resolve(array.get(1).ok_or_else(|| bad("icc_profile"))?)?;
                let dict = profile.get().as_dict().ok_or_else(|| bad("icc_profile"))?;
                basic = match number(resolver, dict, b"N")? {
                    Some(1.0) => Basic::Gray,
                    Some(3.0) => Basic::Rgb,
                    Some(4.0) => Basic::Cmyk,
                    _ => {
                        warn(resolver, profile.object().id(), "render_colorspace_unsupported");
                        Basic::Unknown
                    }
                };
                break;
            }
            if name.eq_bytes(b"Pattern") {
                pattern = true;
                if let Some(base) = array.get(1) {
                    current = resolver.resolve(base)?;
                    continue;
                }
                break;
            }
            if name.eq_bytes(b"Indexed") || name.eq_bytes(b"I") {
                let hival = resolver.resolve(array.get(2).ok_or_else(|| bad("indexed_hival"))?)?;
                let hival = hival.get().as_i64().and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| bad("indexed_hival"))?;
                let lookup = resolver.resolve(array.get(3).ok_or_else(|| bad("indexed_lookup"))?)?;
                let palette = if let Some(string) = lookup.get().as_str() {
                    let mut storage = Vec::new();
                    let bytes = decode_string(
                        string.raw(), &mut storage, resolver.document().limits().max_decoded_bytes,
                    )?;
                    let count = bytes.len().min(256usize.saturating_mul(COMPONENTS));
                    Arc::from(bytes.get(..count).unwrap_or(&[]))
                } else if lookup.object().is_stream() {
                    let mut decoded = resolver.decode_stream(lookup.object())?;
                    if decoded.image.is_some() { return Err(bad("indexed_lookup_codec")); }
                    if decoded.truncated { warn(resolver, lookup.object().id(), "indexed_lookup_truncated"); }
                    decoded.bytes.truncate(256usize.saturating_mul(COMPONENTS));
                    Arc::from(decoded.bytes)
                } else {
                    return Err(bad("indexed_lookup"));
                };
                bounded_push(resolver, &mut mappings, Mapping::Indexed {
                    palette, hival, outputs: 0,
                })?;
                current = resolver.resolve(array.get(1).ok_or_else(|| bad("indexed_base"))?)?;
                continue;
            }
            if name.eq_bytes(b"Separation") || name.eq_bytes(b"DeviceN") {
                let inputs = if name.eq_bytes(b"Separation") {
                    1
                } else {
                    let names = resolver.resolve(array.get(1).ok_or_else(|| bad("devicen_names"))?)?;
                    names.get().as_array().map(|array| array.len())
                        .ok_or_else(|| bad("devicen_names"))?
                };
                check(LimitKind::Tokens, inputs as u64, COMPONENTS as u64)?;
                if inputs == 0 { return Err(bad("tint_components")); }
                let function_value = resolver.resolve(array.get(3).ok_or_else(|| bad("tint_function"))?)?;
                let function = match Type2::load(resolver, function_value.get()) {
                    Ok(function) => Some(function),
                    Err(error @ (DocError::Encrypted | DocError::Refused(_))) => return Err(error),
                    Err(_) => {
                        warn(resolver, current.object().id(), "render_tint_function_unsupported");
                        None
                    }
                };
                bounded_push(resolver, &mut mappings, Mapping::Tint { function, inputs })?;
                current = resolver.resolve(array.get(2).ok_or_else(|| bad("tint_alternate"))?)?;
                continue;
            }
            warn(resolver, current.object().id(), "render_colorspace_unsupported");
            break;
        }
        let mut components = basic.components();
        for mapping in mappings.iter_mut().rev() {
            match mapping {
                Mapping::Indexed { outputs, .. } => {
                    *outputs = components;
                    components = 1;
                }
                Mapping::Tint { inputs, .. } => components = *inputs,
            }
        }
        Ok(Arc::new(Self { basic, mappings, components, pattern }))
    }

    fn color(&self, input: &[f32; COMPONENTS], alpha: f32) -> Rgba {
        let mut values = *input;
        for mapping in &self.mappings {
            match mapping {
                Mapping::Indexed { palette, hival, outputs } => {
                    let index = values.first().copied().unwrap_or(0.0)
                        .clamp(0.0, f32::from(*hival)) as usize;
                    let start = match index.checked_mul(*outputs) {
                        Some(start) => start,
                        None => return gray(alpha),
                    };
                    values.fill(0.0);
                    for (offset, slot) in values.iter_mut().take(*outputs).enumerate() {
                        let Some(index) = start.checked_add(offset) else { return gray(alpha) };
                        let Some(sample) = palette.get(index) else { return gray(alpha) };
                        *slot = f32::from(*sample) / 255.0;
                    }
                }
                Mapping::Tint { function, .. } => {
                    let Some(function) = function else { return gray(alpha) };
                    values = function.evaluate(values.first().copied().unwrap_or(0.0));
                }
            }
        }
        self.basic.color(&values, alpha)
    }
}

#[derive(Clone)]
pub(crate) struct ColorState {
    space: Arc<ColorSpace>,
    values: [f32; COMPONENTS],
    pattern: Option<Value>,
}

impl ColorState {
    pub(crate) fn gray() -> Self { Self::new(ColorSpace::device_gray()) }

    pub(crate) fn new(space: Arc<ColorSpace>) -> Self {
        let mut values = [0.0; COMPONENTS];
        if matches!(space.basic, Basic::Cmyk) && space.mappings.is_empty() {
            values[3] = 1.0;
        }
        Self { space, values, pattern: None }
    }

    pub(crate) fn set(
        &mut self,
        resolver: &mut Resolver<'_>,
        resources: Option<&Value>,
        operands: &[Operand<'_>],
    ) -> Result<(), DocError> {
        let (components, pattern) = if self.space.pattern {
            let Some((Operand::Name(name), components)) = operands.split_last() else {
                return Err(bad("pattern_color"));
            };
            let mut storage = Vec::new();
            let name = decode_name(name, &mut storage, resolver.document().limits().max_object_bytes)?;
            let pattern = resource(resolver, resources, b"Pattern", name)?;
            if pattern.is_none() { warn(resolver, None, "render_pattern_missing"); }
            (components, pattern)
        } else {
            (operands, None)
        };
        if components.len() != self.space.components { return Err(bad("color_components")); }
        let mut values = [0.0; COMPONENTS];
        for (slot, component) in values.iter_mut().zip(components) {
            let Operand::Number(value) = component else { return Err(bad("color_component")) };
            *slot = finite(*value).ok_or_else(|| bad("color_component"))?;
        }
        self.values = values;
        self.pattern = pattern;
        Ok(())
    }

    pub(crate) fn paint(
        &self,
        resolver: &mut Resolver<'_>,
        ctm: Matrix,
        alpha: f32,
    ) -> Result<Paint, DocError> {
        if let Some(pattern) = &self.pattern {
            let dict = pattern.get().as_dict().ok_or_else(|| bad("pattern_dictionary"))?;
            if number(resolver, dict, b"PatternType")? == Some(2.0) {
                if let Some(value) = resolver.resolve_dict_key(dict, b"Shading")? {
                    let matrix = read_array::<6>(resolver, dict, b"Matrix")?
                        .map(Matrix).unwrap_or(Matrix::IDENTITY).then(ctm);
                    return Ok(shading(resolver, value.get(), matrix, alpha)?.paint);
                }
            }
            warn(resolver, pattern.object().id(), "render_pattern_unsupported");
            return Ok(Paint::Solid(gray(alpha)));
        }
        Ok(Paint::Solid(self.space.color(&self.values, alpha)))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Type2 {
    c0: [f32; COMPONENTS],
    c1: [f32; COMPONENTS],
    outputs: usize,
    exponent: f32,
    domain: [f32; 2],
    range: Option<([f32; COMPONENTS], [f32; COMPONENTS])>,
}

impl Type2 {
    fn load(resolver: &mut Resolver<'_>, value: ValueRef<'_>) -> Result<Self, DocError> {
        let value = resolver.resolve(value)?;
        let dict = value.get().as_dict().ok_or_else(|| bad("function_dictionary"))?;
        if number(resolver, dict, b"FunctionType")? != Some(2.0) {
            return Err(bad("function_type2"));
        }
        let exponent = number(resolver, dict, b"N")?.ok_or_else(|| bad("function_exponent"))?;
        if exponent < 0.0 { return Err(bad("function_exponent")); }
        let domain = read_array::<2>(resolver, dict, b"Domain")?.unwrap_or([0.0, 1.0]);
        if domain[0] > domain[1] || domain[0] < 0.0 { return Err(bad("function_domain")); }
        let (c0, count0) = components(resolver, dict, b"C0", 0.0)?;
        let (c1, count1) = components(resolver, dict, b"C1", 1.0)?;
        if count0 != count1 || count0 == 0 { return Err(bad("function_components")); }
        let range = if let Some(value) = resolver.resolve_dict_key(dict, b"Range")? {
            let array = value.get().as_array().ok_or_else(|| bad("function_range"))?;
            if array.len() != count0.saturating_mul(2) { return Err(bad("function_range")); }
            let mut low = [0.0; COMPONENTS];
            let mut high = [0.0; COMPONENTS];
            let mut values = array.iter();
            for (low, high) in low.iter_mut().zip(high.iter_mut()).take(count0) {
                let a = resolver.resolve(values.next().ok_or_else(|| bad("function_range"))?)?;
                let b = resolver.resolve(values.next().ok_or_else(|| bad("function_range"))?)?;
                *low = a.get().as_f64().and_then(finite).ok_or_else(|| bad("function_range"))?;
                *high = b.get().as_f64().and_then(finite).ok_or_else(|| bad("function_range"))?;
                if *low > *high { return Err(bad("function_range")); }
            }
            Some((low, high))
        } else {
            None
        };
        Ok(Self { c0, c1, outputs: count0, exponent, domain, range })
    }

    fn evaluate(&self, input: f32) -> [f32; COMPONENTS] {
        let input = input.clamp(self.domain[0], self.domain[1]);
        let factor = power(f64::from(input), f64::from(self.exponent));
        let mut result = [0.0; COMPONENTS];
        for (index, slot) in result.iter_mut().enumerate().take(self.outputs) {
            let a = self.c0.get(index).copied().unwrap_or(0.0);
            let b = self.c1.get(index).copied().unwrap_or(1.0);
            let value = f64::from(a) * (1.0 - factor) + f64::from(b) * factor;
            *slot = finite(value).unwrap_or_else(|| if value.is_sign_negative() { 0.0 } else { 1.0 });
            if let Some((low, high)) = &self.range {
                let low = low.get(index).copied().unwrap_or(0.0);
                let high = high.get(index).copied().unwrap_or(1.0);
                *slot = slot.clamp(low, high);
            }
        }
        result
    }
}

struct Segment {
    from: f32,
    to: f32,
    encode: [f32; 2],
    function: Type2,
}

enum Function {
    Exponential(Type2),
    Stitch { segments: Vec<Segment>, domain: [f32; 2] },
    Channels(Vec<Type2>),
}

impl Function {
    fn load(resolver: &mut Resolver<'_>, value: ValueRef<'_>) -> Result<Self, DocError> {
        let value = resolver.resolve(value)?;
        if let Some(array) = value.get().as_array() {
            check(LimitKind::Tokens, array.len() as u64, COMPONENTS as u64)?;
            let mut functions = Vec::new();
            for value in array.iter() {
                let function = Type2::load(resolver, value)?;
                if function.outputs != 1 { return Err(bad("function_channel")); }
                bounded_push(resolver, &mut functions, function)?;
            }
            if functions.is_empty() { return Err(bad("function_channels")); }
            return Ok(Self::Channels(functions));
        }
        let dict = value.get().as_dict().ok_or_else(|| bad("function_dictionary"))?;
        match number(resolver, dict, b"FunctionType")? {
            Some(2.0) => Ok(Self::Exponential(Type2::load(resolver, value.get())?)),
            Some(3.0) => {
                let domain = read_array::<2>(resolver, dict, b"Domain")?
                    .ok_or_else(|| bad("stitch_domain"))?;
                if domain[0] >= domain[1] { return Err(bad("stitch_domain")); }
                let functions = resolver.resolve_dict_key(dict, b"Functions")?
                    .ok_or_else(|| bad("stitch_functions"))?;
                let functions = functions.get().as_array().ok_or_else(|| bad("stitch_functions"))?;
                check(
                    LimitKind::Tokens,
                    functions.len() as u64,
                    u64::from(resolver.document().limits().max_tokens_per_object),
                )?;
                if functions.is_empty() { return Err(bad("stitch_functions")); }
                let bounds = resolver.resolve_dict_key(dict, b"Bounds")?
                    .ok_or_else(|| bad("stitch_bounds"))?;
                let bounds = bounds.get().as_array().ok_or_else(|| bad("stitch_bounds"))?;
                let encode = resolver.resolve_dict_key(dict, b"Encode")?
                    .ok_or_else(|| bad("stitch_encode"))?;
                let encode = encode.get().as_array().ok_or_else(|| bad("stitch_encode"))?;
                if bounds.len() != functions.len().saturating_sub(1)
                    || encode.len() != functions.len().saturating_mul(2)
                {
                    return Err(bad("stitch_lengths"));
                }
                let mut segments = Vec::new();
                let mut from = domain[0];
                let mut outputs = None;
                let mut bounds = bounds.iter();
                let mut encoded = encode.iter();
                for (index, value) in functions.iter().enumerate() {
                    let to = if index.saturating_add(1) == functions.len() {
                        domain[1]
                    } else {
                        resolved_number(resolver, bounds.next().ok_or_else(|| bad("stitch_bounds"))?)?
                    };
                    if from >= to || to > domain[1] { return Err(bad("stitch_bounds")); }
                    let e0 = resolved_number(resolver, encoded.next().ok_or_else(|| bad("stitch_encode"))?)?;
                    let e1 = resolved_number(resolver, encoded.next().ok_or_else(|| bad("stitch_encode"))?)?;
                    let function = Type2::load(resolver, value)?;
                    if outputs.is_some_and(|outputs| outputs != function.outputs) {
                        return Err(bad("stitch_components"));
                    }
                    outputs = Some(function.outputs);
                    bounded_push(resolver, &mut segments, Segment {
                        from, to, encode: [e0, e1], function,
                    })?;
                    from = to;
                }
                Ok(Self::Stitch { segments, domain })
            }
            _ => Err(bad("shading_function_unsupported")),
        }
    }

    fn evaluate(&self, input: f32) -> [f32; COMPONENTS] {
        match self {
            Self::Exponential(function) => function.evaluate(input),
            Self::Channels(functions) => {
                let mut result = [0.0; COMPONENTS];
                for (slot, function) in result.iter_mut().zip(functions) {
                    *slot = function.evaluate(input).first().copied().unwrap_or(0.0);
                }
                result
            }
            Self::Stitch { segments, domain } => {
                let input = input.clamp(domain[0], domain[1]);
                let segment = segments.iter().find(|segment| input < segment.to)
                    .or_else(|| segments.last());
                segment.map(|segment| evaluate_segment(segment, input))
                    .unwrap_or([0.5; COMPONENTS])
            }
        }
    }
}

/// Function-based shading evaluated in device space by the rasterizer.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionShading {
    inverse: [f64; 6],
    bbox: Option<([f64; 6], [f32; 4])>,
    domain: [f32; 4],
    function_domain: [f32; 4],
    range: Vec<(f32, f32)>,
    calculator: crate::calculator::Calculator,
    space: Arc<ColorSpace>,
    alpha: f32,
    background: Option<Rgba>,
}

impl FunctionShading {
    pub fn sample(&self, x: f32, y: f32, work: &mut u64) -> Result<Option<Rgba>, DocError> {
        let point = |matrix: [f64; 6]| {
            let [a, b, c, d, e, f] = matrix;
            let x = f64::from(x);
            let y = f64::from(y);
            [a * x + c * y + e, b * x + d * y + f]
        };
        let contains = |[x, y]: [f64; 2], bounds: [f32; 4]| {
            x.is_finite() && y.is_finite()
                && x >= f64::from(bounds[0]) && x <= f64::from(bounds[2])
                && y >= f64::from(bounds[1]) && y <= f64::from(bounds[3])
        };
        if self.bbox.is_some_and(|(matrix, bbox)| !contains(point(matrix), bbox)) {
            return Ok(None);
        }
        let [x, y] = point(self.inverse);
        let [x0, x1, y0, y1] = self.domain;
        if !contains([x, y], [x0, y0, x1, y1]) { return Ok(self.background); }
        let domain = self.function_domain;
        let values = self.calculator.evaluate([
            x.clamp(f64::from(domain[0]), f64::from(domain[1])),
            y.clamp(f64::from(domain[2]), f64::from(domain[3])),
        ], self.range.len(), work)?;
        let mut components = [0.0; COMPONENTS];
        for ((slot, value), (low, high)) in components.iter_mut().zip(values).zip(&self.range) {
            *slot = finite(value.clamp(f64::from(*low), f64::from(*high)))
                .ok_or_else(|| bad("calculator_output"))?;
        }
        Ok(Some(self.space.color(&components, self.alpha)))
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.calculator.allocated_bytes().saturating_add(
            self.range.capacity().saturating_mul(core::mem::size_of::<(f32, f32)>()),
        ).saturating_add(core::mem::size_of::<Self>())
    }
}

fn function_shading(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    matrix: Matrix,
    bbox: Option<[f32; 4]>,
    space: Arc<ColorSpace>,
    alpha: f32,
) -> Result<Shading, DocError> {
    let domain = read_array::<4>(resolver, dict, b"Domain")?.unwrap_or([0.0, 1.0, 0.0, 1.0]);
    if domain[0] >= domain[1] || domain[2] >= domain[3] { return Err(bad("shading_domain")); }
    let local = read_array::<6>(resolver, dict, b"Matrix")?.map(Matrix).unwrap_or(Matrix::IDENTITY);
    let inverse = inverse_matrix(local.then(matrix))?;
    let bbox = bbox.map(|bbox| -> Result<_, DocError> {
        if bbox[0] > bbox[2] || bbox[1] > bbox[3] { return Err(bad("shading_bbox")); }
        Ok((inverse_matrix(matrix)?, bbox))
    }).transpose()?;
    let value = resolver.resolve_dict_key(dict, b"Function")?
        .ok_or_else(|| bad("shading_function"))?;
    if value.get().as_array().is_some() {
        return Err(DocError::Unsupported { feature: "type1 shading function array" });
    }
    let function = value.get().as_dict().ok_or_else(|| bad("function_dictionary"))?;
    if number(resolver, function, b"FunctionType")? != Some(4.0) {
        return Err(DocError::Unsupported { feature: "type1 shading function (requires type4)" });
    }
    if !value.object().is_stream() { return Err(bad("calculator_stream")); }
    let function_domain = read_array::<4>(resolver, function, b"Domain")?
        .ok_or_else(|| bad("calculator_domain"))?;
    if function_domain[0] > function_domain[1] || function_domain[2] > function_domain[3] {
        return Err(bad("calculator_domain"));
    }
    let ranges = resolver.resolve_dict_key(function, b"Range")?
        .ok_or_else(|| bad("calculator_range"))?;
    let ranges = ranges.get().as_array().ok_or_else(|| bad("calculator_range"))?;
    check(LimitKind::Tokens, ranges.len() as u64, (COMPONENTS * 2) as u64)?;
    if space.pattern || space.components == 0 || ranges.len() != space.components.saturating_mul(2) {
        return Err(bad("calculator_components"));
    }
    let mut range = Vec::new();
    let mut pairs = ranges.iter();
    while let Some(low) = pairs.next() {
        let high = pairs.next().ok_or_else(|| bad("calculator_range"))?;
        let low = resolved_number(resolver, low)?;
        let high = resolved_number(resolver, high)?;
        if low > high { return Err(bad("calculator_range")); }
        bounded_push(resolver, &mut range, (low, high))?;
    }
    let background = if resolver.resolve_dict_key(dict, b"Background")?.is_some() {
        let (values, count) = components(resolver, dict, b"Background", 0.0)?;
        if count != space.components { return Err(bad("shading_background")); }
        Some(space.color(&values, alpha))
    } else {
        None
    };
    let decoded = resolver.decode_stream(value.object())?;
    if decoded.truncated || decoded.image.is_some() { return Err(bad("calculator_stream")); }
    let calculator = crate::calculator::Calculator::parse(
        &decoded.bytes, *resolver.document().limits(),
    )?;
    let shading = FunctionShading {
        inverse, bbox, domain, function_domain, range, calculator, space, alpha, background,
    };
    let requested = (shading.allocated_bytes() as u64).saturating_add(decoded.bytes.len() as u64);
    check(LimitKind::DecodedBytes, requested, resolver.document().limits().max_decoded_bytes)?;
    check(LimitKind::TotalDecodedBytes, requested, resolver.document().limits().max_total_decoded_bytes)?;
    Ok(Shading { paint: Paint::Function(Arc::new(shading)), bbox: None })
}

fn inverse_matrix(matrix: Matrix) -> Result<[f64; 6], DocError> {
    if !matrix.is_finite() { return Err(bad("shading_matrix")); }
    let [a, b, c, d, e, f] = matrix.0.map(f64::from);
    let determinant = a * d - b * c;
    if !determinant.is_finite() || determinant == 0.0 { return Err(bad("shading_matrix")); }
    let result = [
        d / determinant, -b / determinant, -c / determinant, a / determinant,
        (c * f - d * e) / determinant, (b * e - a * f) / determinant,
    ];
    if !result.iter().all(|value| value.is_finite()) { return Err(bad("shading_matrix")); }
    Ok(result)
}

pub(crate) struct Shading {
    pub(crate) paint: Paint,
    pub(crate) bbox: Option<[f32; 4]>,
}

pub(crate) fn shading(
    resolver: &mut Resolver<'_>,
    value: ValueRef<'_>,
    matrix: Matrix,
    alpha: f32,
) -> Result<Shading, DocError> {
    if !matrix.is_finite() { return Err(bad("shading_matrix")); }
    let value = resolver.resolve(value)?;
    let dict = value.get().as_dict().ok_or_else(|| bad("shading_dictionary"))?;
    let bbox = read_array::<4>(resolver, dict, b"BBox")?;
    let space = match resolver.resolve_dict_key(dict, b"ColorSpace")? {
        Some(space) => ColorSpace::load(resolver, None, space.get())?,
        None => ColorSpace::basic(Basic::Unknown),
    };
    let kind = number(resolver, dict, b"ShadingType")?;
    if kind == Some(1.0) {
        return function_shading(resolver, dict, matrix, bbox, space, alpha);
    }
    let function = match resolver.resolve_dict_key(dict, b"Function")? {
        Some(value) => match Function::load(resolver, value.get()) {
            Ok(function) => Some(function),
            Err(error @ (DocError::Encrypted | DocError::Refused(_))) => return Err(error),
            Err(_) => {
                warn(resolver, value.object().id(), "render_shading_function_unsupported");
                None
            }
        },
        None => None,
    };
    let domain = read_array::<2>(resolver, dict, b"Domain")?.unwrap_or([0.0, 1.0]);
    if domain[0] >= domain[1] { return Err(bad("shading_domain")); }
    let midpoint = (f64::from(domain[0]) * 0.5 + f64::from(domain[1]) * 0.5) as f32;
    let mean = function.as_ref().map(|function| space.color(&function.evaluate(midpoint), alpha))
        .unwrap_or_else(|| gray(alpha));
    if !matches!(kind, Some(2.0) | Some(3.0)) || function.is_none() {
        warn(resolver, value.object().id(), "render_shading_approximated");
        return Ok(Shading { paint: Paint::Solid(mean), bbox });
    }
    let mut extend = (false, false);
    if let Some(value) = resolver.resolve_dict_key(dict, b"Extend")? {
        let array = value.get().as_array().filter(|array| array.len() == 2)
            .ok_or_else(|| bad("shading_extend"))?;
        let a = resolver.resolve(array.get(0).ok_or_else(|| bad("shading_extend"))?)?;
        let b = resolver.resolve(array.get(1).ok_or_else(|| bad("shading_extend"))?)?;
        extend = (
            a.get().as_bool().ok_or_else(|| bad("shading_extend"))?,
            b.get().as_bool().ok_or_else(|| bad("shading_extend"))?,
        );
    }
    let Some(function) = function else {
        return Ok(Shading { paint: Paint::Solid(mean), bbox });
    };
    let mut stops = Vec::new();
    for index in 0..=SAMPLES {
        let position = index as f32 / SAMPLES as f32;
        let input = interpolate(domain[0], domain[1], position);
        bounded_push(resolver, &mut stops, (
            position, space.color(&function.evaluate(input), alpha),
        ))?;
    }
    if let Function::Stitch { segments, .. } = &function {
        for pair in segments.windows(2) {
            let (Some(left), Some(right)) = (pair.first(), pair.get(1)) else { continue };
            let position = ((f64::from(left.to) - f64::from(domain[0]))
                / (f64::from(domain[1]) - f64::from(domain[0]))) as f32;
            if (0.0..=1.0).contains(&position) {
                bounded_push(resolver, &mut stops, (
                    position, space.color(&evaluate_segment(left, left.to), alpha),
                ))?;
                bounded_push(resolver, &mut stops, (
                    position, space.color(&evaluate_segment(right, right.from), alpha),
                ))?;
            }
        }
        stops.sort_by(|left, right| left.0.total_cmp(&right.0));
    }
    let paint = if kind == Some(2.0) {
        let [x0, y0, x1, y1] = read_array::<4>(resolver, dict, b"Coords")?
            .ok_or_else(|| bad("axial_coordinates"))?;
        Paint::Axial {
            from: checked_point(matrix, x0, y0)?,
            to: checked_point(matrix, x1, y1)?,
            stops, extend,
        }
    } else {
        let [x0, y0, r0, x1, y1, r1] = read_array::<6>(resolver, dict, b"Coords")?
            .ok_or_else(|| bad("radial_coordinates"))?;
        if r0 < 0.0 || r1 < 0.0 { return Err(bad("radial_radius")); }
        let from = checked_point(matrix, x0, y0)?;
        let to = checked_point(matrix, x1, y1)?;
        let scale = matrix.mean_scale();
        let r0 = r0 * scale;
        let r1 = r1 * scale;
        if !r0.is_finite() || !r1.is_finite() { return Err(bad("radial_geometry")); }
        Paint::Radial {
            from: (from.0, from.1, r0), to: (to.0, to.1, r1), stops, extend,
        }
    };
    Ok(Shading { paint, bbox })
}

fn components(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
    default: f32,
) -> Result<([f32; COMPONENTS], usize), DocError> {
    let mut result = [0.0; COMPONENTS];
    let Some(value) = resolver.resolve_dict_key(dict, key)? else {
        result[0] = default;
        return Ok((result, 1));
    };
    let array = value.get().as_array().ok_or_else(|| bad("function_components"))?;
    check(LimitKind::Tokens, array.len() as u64, COMPONENTS as u64)?;
    for (slot, value) in result.iter_mut().zip(array.iter()) {
        *slot = resolved_number(resolver, value)?;
    }
    Ok((result, array.len()))
}

fn resolved_number(resolver: &mut Resolver<'_>, value: ValueRef<'_>) -> Result<f32, DocError> {
    let value = resolver.resolve(value)?;
    value.get().as_f64().and_then(finite).ok_or_else(|| bad("function_number"))
}

fn evaluate_segment(segment: &Segment, input: f32) -> [f32; COMPONENTS] {
    let fraction = ((f64::from(input) - f64::from(segment.from))
        / (f64::from(segment.to) - f64::from(segment.from))) as f32;
    segment.function.evaluate(interpolate(
        segment.encode[0], segment.encode[1], fraction.clamp(0.0, 1.0),
    ))
}

fn interpolate(from: f32, to: f32, position: f32) -> f32 {
    (f64::from(from) * (1.0 - f64::from(position))
        + f64::from(to) * f64::from(position)) as f32
}

fn named_basic(name: &[u8]) -> Option<Basic> {
    match name {
        b"DeviceGray" | b"G" | b"CalGray" => Some(Basic::Gray),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(Basic::Rgb),
        b"DeviceCMYK" | b"CMYK" => Some(Basic::Cmyk),
        _ => None,
    }
}

fn gray(alpha: f32) -> Rgba {
    Rgba { r: 0.5, g: 0.5, b: 0.5, a: unit(alpha) }
}

fn unit(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 }
}

fn bounded_push<T>(
    resolver: &Resolver<'_>,
    values: &mut Vec<T>,
    value: T,
) -> Result<(), DocError> {
    let maximum = resolver.document().limits().max_decoded_bytes;
    let requested = values.len().checked_add(1)
        .and_then(|count| count.checked_mul(core::mem::size_of::<T>()))
        .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, maximum))?;
    check(LimitKind::DecodedBytes, requested as u64, maximum.min(isize::MAX as u64))?;
    values.try_reserve(1).map_err(|_| {
        refusal(LimitKind::DecodedBytes, maximum.saturating_add(1), maximum)
    })?;
    values.push(value);
    Ok(())
}

/// Bounded log/exp approximation for exponential interpolation in no_std.
/// Input is nonnegative and the PDF exponent has already been validated.
fn power(mut value: f64, exponent: f64) -> f64 {
    if exponent == 0.0 { return 1.0; }
    if value == 0.0 { return 0.0; }
    if exponent == 1.0 { return value; }
    if exponent == 2.0 { return value * value; }
    let mut shift = 0i32;
    for _ in 0..160 {
        if value >= 2.0 {
            value *= 0.5;
            shift = shift.saturating_add(1);
        } else if value < 1.0 {
            value *= 2.0;
            shift = shift.saturating_sub(1);
        } else {
            break;
        }
    }
    let ratio = (value - 1.0) / (value + 1.0);
    let square = ratio * ratio;
    let mut term = ratio;
    let mut logarithm = 0.0;
    for index in 0..24u32 {
        logarithm += term / f64::from(index.saturating_mul(2).saturating_add(1));
        term *= square;
    }
    logarithm = logarithm * 2.0 + f64::from(shift) * core::f64::consts::LN_2;
    let argument = logarithm * exponent;
    if argument < -745.0 { return 0.0; }
    if argument > 709.0 { return f64::MAX; }
    let integral = (argument / core::f64::consts::LN_2) as i32;
    let remainder = argument - f64::from(integral) * core::f64::consts::LN_2;
    let mut term = 1.0;
    let mut result = 1.0;
    for index in 1..=24u32 {
        term *= remainder / f64::from(index);
        result += term;
    }
    for _ in 0..integral.unsigned_abs().min(1075) {
        result *= if integral >= 0 { 2.0 } else { 0.5 };
    }
    result
}
