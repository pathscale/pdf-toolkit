use alloc::string::String;
use alloc::vec::Vec;

use pdf_core::{DictRef, DocError, LimitKind, Object, Resolver, Value};

use crate::content::check;
use crate::links::{read_key, recover, text_string, warn};
use crate::semantic::Budget;

const MAX_ITEMS: u64 = 10_000;
const MAX_STEPS: u64 = 100_000;
const MAX_NAME_BYTES: u64 = 16 << 10;

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    pub index: usize,
    pub subtype: String,
    pub rect: [f32; 4],
    pub contents: Option<String>,
    pub color: Option<[f32; 3]>,
    pub flags: u32,
    pub has_appearance: bool,
}

/// Indices retain their positions in /Annots, including skipped entries and Links.
pub fn page_annotations(resolver: &mut Resolver<'_>, page: &Object) -> Vec<Annotation> {
    let maximum = resolver.document().limits().max_total_decoded_bytes;
    let result = Context {
        resolver,
        budget: Budget::new(maximum),
        steps: 0,
    }
    .read(page);
    match result {
        Ok(annotations) => annotations,
        Err(error) => {
            warn(resolver, page.id(), error.code());
            Vec::new()
        }
    }
}

struct Context<'r, 'd> {
    resolver: &'r mut Resolver<'d>,
    budget: Budget,
    steps: u64,
}

impl Context<'_, '_> {
    fn step(&mut self) -> Result<(), DocError> {
        self.steps = self.steps.saturating_add(1);
        check(
            LimitKind::ContentOps,
            self.steps,
            self.resolver
                .document()
                .limits()
                .max_content_ops
                .min(MAX_STEPS),
        )
    }

    fn key(&mut self, dict: DictRef<'_>, key: &[u8]) -> Result<Option<Value>, DocError> {
        self.step()?;
        read_key(self.resolver, dict, key)
    }

    fn numbers<const N: usize>(&mut self, value: &Value) -> Result<[f32; N], DocError> {
        let array = value
            .get()
            .as_array()
            .filter(|array| array.len() == N)
            .ok_or(DocError::WrongType {
                id: value.object().id(),
                expected: "annotation number array",
            })?;
        let mut out = [0.0; N];
        for (slot, value) in out.iter_mut().zip(array.iter()) {
            self.step()?;
            let value = self.resolver.resolve(value)?;
            let number = value
                .get()
                .as_f64()
                .filter(|number| number.is_finite() && number.abs() <= f64::from(f32::MAX))
                .ok_or(DocError::WrongType {
                    id: value.object().id(),
                    expected: "finite annotation number",
                })?;
            *slot = number as f32;
        }
        Ok(out)
    }

    fn appearance(&mut self, dict: DictRef<'_>) -> Result<bool, DocError> {
        let Some(ap) = self.key(dict, b"AP")? else {
            return Ok(false);
        };
        let Some(ap) = ap.get().as_dict() else {
            return Ok(false);
        };
        let Some(normal) = self.key(ap, b"N")? else {
            return Ok(false);
        };
        if normal.object().is_stream() {
            return Ok(true);
        }
        let Some(states) = normal.get().as_dict() else {
            return Ok(false);
        };
        let Some(state) = self.key(dict, b"AS")? else {
            return Ok(false);
        };
        let Some(name) = state.get().as_name() else {
            return Ok(false);
        };
        check(
            LimitKind::ObjectBytes,
            name.encoded().len() as u64,
            self.resolver
                .document()
                .limits()
                .max_object_bytes
                .min(MAX_NAME_BYTES),
        )?;
        let name = name.decoded();
        Ok(self
            .key(states, name.as_ref())?
            .is_some_and(|value| value.object().is_stream()))
    }

    fn annotation(&mut self, value: &Value, index: usize) -> Result<Option<Annotation>, DocError> {
        let Some(dict) = value.get().as_dict() else {
            return Ok(None);
        };
        let Some(subtype) = self.key(dict, b"Subtype")? else {
            return Ok(None);
        };
        let Some(name) = subtype.get().as_name() else {
            return Ok(None);
        };
        check(
            LimitKind::ObjectBytes,
            name.encoded().len() as u64,
            self.resolver
                .document()
                .limits()
                .max_object_bytes
                .min(MAX_NAME_BYTES),
        )?;
        let name = name.decoded();
        self.budget.charge((name.len() as u64).saturating_mul(3))?;
        let subtype = String::from_utf8_lossy(name.as_ref()).into_owned();
        let Some(rect) = self.key(dict, b"Rect")? else {
            return Ok(None);
        };
        let [x0, y0, x1, y1] = self.numbers::<4>(&rect)?;
        let rect = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
        let contents = match self.key(dict, b"Contents")? {
            Some(text) => {
                let limits = *self.resolver.document().limits();
                let result = text_string(self.resolver, &text, limits);
                recover(
                    self.resolver,
                    result,
                    text.object().id(),
                    "annotation_contents_invalid",
                )?
                .flatten()
            }
            None => None,
        };
        if let Some(text) = &contents {
            self.budget.charge(text.len() as u64)?;
        }
        let color = match self.key(dict, b"C")? {
            Some(value) if value.get().as_array().is_some_and(|array| array.len() == 3) => {
                Some(self.numbers::<3>(&value)?)
            }
            _ => None,
        };
        let flags = self
            .key(dict, b"F")?
            .and_then(|value| value.get().as_i64())
            .and_then(|number| u32::try_from(number).ok())
            .unwrap_or(0);
        let has_appearance = self.appearance(dict)?;
        Ok(Some(Annotation {
            index,
            subtype,
            rect,
            contents,
            color,
            flags,
            has_appearance,
        }))
    }

    fn read(&mut self, page: &Object) -> Result<Vec<Annotation>, DocError> {
        if !self.resolver.document().decryptable() {
            return Err(DocError::Encrypted);
        }
        let Some(dict) = page.dict() else {
            return Ok(Vec::new());
        };
        let Some(annots) = self.key(dict, b"Annots")? else {
            return Ok(Vec::new());
        };
        let Some(array) = annots.get().as_array() else {
            warn(self.resolver, page.id(), "annotation_array_invalid");
            return Ok(Vec::new());
        };
        check(
            LimitKind::Objects,
            array.len() as u64,
            u64::from(self.resolver.document().limits().max_objects).min(MAX_ITEMS),
        )?;
        let mut out = Vec::new();
        for (index, annotation) in array.iter().enumerate() {
            self.step()?;
            let result = self.resolver.resolve(annotation);
            let Some(value) = recover(
                self.resolver,
                result,
                annotation.as_ref(),
                "annotation_object_failed",
            )?
            else {
                continue;
            };
            let result = self.annotation(&value, index);
            let Some(Some(annotation)) = recover(
                self.resolver,
                result,
                value.object().id(),
                "annotation_invalid",
            )?
            else {
                continue;
            };
            self.budget.push(&mut out, annotation)?;
        }
        Ok(out)
    }
}
