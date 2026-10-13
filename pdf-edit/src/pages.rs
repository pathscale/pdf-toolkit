use std::collections::BTreeSet;

use pdf_core::pages::PageWalk;
use pdf_core::{DocError, Document, LimitKind, ObjectRef};
use pdf_write::{Dict, Value};

use crate::{check, get, malformed, refused, set, value, wrong, Editor, Pending};

#[derive(Clone)]
pub(crate) struct PageEntry {
    pub(crate) id: ObjectRef,
    pub(crate) inherited: Dict,
}

pub(crate) fn dictionary(document: &Document, id: ObjectRef) -> Result<Dict, DocError> {
    let object = document.resolver().get(id)?;
    if object.is_stream() {
        return Err(wrong(Some(id), "page dictionary"));
    }
    match value::checked_object(&object, document.limits())? {
        Value::Dict(dict) => Ok(dict),
        _ => Err(wrong(Some(id), "page dictionary")),
    }
}

fn box_value(numbers: [f64; 4]) -> Value {
    Value::Array(numbers.into_iter().map(Value::Real).collect())
}

pub(crate) fn initial(document: &Document) -> Result<(ObjectRef, Vec<PageEntry>), DocError> {
    let root = document.root_ref().ok_or_else(|| malformed("catalog"))?;
    let catalog = document.resolver().get(root)?;
    let pages_root = catalog
        .dict()
        .and_then(|dict| dict.get(b"Pages"))
        .and_then(|value| value.as_ref())
        .ok_or_else(|| malformed("pages"))?;
    let mut resolver = document.resolver();
    let mut out = Vec::new();
    let mut retained = 0u64;
    let maximum = document.limits().max_total_decoded_bytes;
    for page in PageWalk::new(&mut resolver)? {
        let page = page?;
        let mut inherited = Vec::new();
        let mut next = Some(page.id);
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            let depth = (seen.len() as u64).saturating_add(1);
            check(
                LimitKind::PageTreeDepth,
                depth,
                u64::from(document.limits().max_page_tree_depth).saturating_add(1),
            )?;
            if !seen.insert((id.number(), id.generation())) {
                return Err(malformed("page Parent cycle"));
            }
            let dict = dictionary(document, id)?;
            for key in ["Resources", "MediaBox", "CropBox", "Rotate"] {
                if get(&inherited, key).is_none() {
                    if let Some(found) =
                        get(&dict, key).filter(|value| !matches!(value, Value::Null))
                    {
                        set(&mut inherited, key, found.clone());
                    }
                }
            }
            if inherited.len() == 4 {
                break;
            }
            next = match get(&dict, "Parent") {
                None | Some(Value::Null) => None,
                Some(Value::Ref(number, generation)) => Some(ObjectRef::new(*number, *generation)),
                _ => return Err(wrong(Some(id), "page Parent reference")),
            };
        }
        for (key, default) in [
            ("Resources", Value::Dict(Vec::new())),
            ("MediaBox", box_value(page.media_box)),
            ("CropBox", box_value(page.crop_box)),
            ("Rotate", Value::Int(i64::from(page.rotate))),
        ] {
            if get(&inherited, key).is_none() {
                set(&mut inherited, key, default);
            }
        }
        let body = Value::Dict(inherited);
        let cost = value::measure(&body, document.limits())?;
        retained = value::add(
            retained,
            cost.saturating_add(core::mem::size_of::<PageEntry>() as u64),
            LimitKind::TotalDecodedBytes,
            maximum,
        )?;
        out.try_reserve(1).map_err(|_| {
            refused(
                LimitKind::TotalDecodedBytes,
                maximum.saturating_add(1),
                maximum,
            )
        })?;
        let Value::Dict(inherited) = body else {
            return Err(malformed("page attributes"));
        };
        out.push(PageEntry {
            id: page.id,
            inherited,
        });
    }
    Ok((pages_root, out))
}

pub(crate) fn explicit(dict: &mut Dict, entry: &PageEntry) {
    for (key, inherited) in &entry.inherited {
        if get(dict, key).is_none_or(|value| matches!(value, Value::Null)) {
            set(dict, key, inherited.clone());
        }
    }
}

impl Editor {
    pub fn page_count(&self) -> u32 {
        u32::try_from(self.pages.len()).unwrap_or(u32::MAX)
    }

    pub fn rotate_page(&mut self, page: u32, quarter_turns: i32) -> Result<(), DocError> {
        self.clean()?;
        let id = self.page_ref(page)?;
        let entry = self
            .pages
            .get(page as usize)
            .ok_or_else(|| malformed("page index"))?;
        let mut dict = self.dict_at(id)?;
        explicit(&mut dict, entry);
        let mut rotation = get(&dict, "Rotate").cloned().unwrap_or(Value::Int(0));
        let mut seen = BTreeSet::new();
        while let Value::Ref(number, generation) = rotation {
            let maximum = u64::from(self.document.limits().max_ref_chain);
            check(
                LimitKind::RefChain,
                (seen.len() as u64).saturating_add(1),
                maximum,
            )?;
            if !seen.insert((number, generation)) {
                return Err(refused(
                    LimitKind::RefChain,
                    maximum.saturating_add(1),
                    maximum,
                ));
            }
            rotation = self
                .read_value(ObjectRef::new(number, generation))?
                .ok_or_else(|| malformed("page Rotate"))?;
        }
        let degrees = match rotation {
            Value::Int(value) => value as f64,
            Value::Real(value) if value.is_finite() => value,
            _ => return Err(malformed("page Rotate")),
        };
        let quarter = degrees.clamp(-1.0e9, 1.0e9) / 90.0;
        let nearest = if quarter >= 0.0 {
            (quarter + 0.5) as i64
        } else {
            (quarter - 0.5) as i64
        };
        let rotation =
            (nearest.rem_euclid(4) + i64::from(quarter_turns).rem_euclid(4)).rem_euclid(4) * 90;
        set(&mut dict, "Rotate", Value::Int(rotation));
        let body = Value::Dict(dict);
        let cost = self.cost(id, &body, None)?;
        let total = self.changed_size(&[(id, cost)])?;
        self.install(Pending {
            id,
            value: body,
            data: None,
            cost,
        });
        self.pending_bytes = total;
        Ok(())
    }

    pub fn delete_page(&mut self, page: u32) -> Result<(), DocError> {
        self.clean()?;
        self.page_ref(page)?;
        if self.pages.len() <= 1 {
            return Err(malformed("cannot delete last page"));
        }
        self.pages.remove(page as usize);
        self.order_changed = true;
        Ok(())
    }

    /// `to` is the final index of the moved page.
    pub fn move_page(&mut self, from: u32, to: u32) -> Result<(), DocError> {
        self.clean()?;
        self.page_ref(from)?;
        self.page_ref(to)?;
        if from != to {
            let page = self.pages.remove(from as usize);
            self.pages.insert(to as usize, page);
            self.order_changed = true;
        }
        Ok(())
    }

    pub(crate) fn page_updates(&self) -> Result<Vec<Pending>, DocError> {
        if !self.order_changed {
            return Ok(Vec::new());
        }
        let limits = self.document.limits();
        check(
            LimitKind::Pages,
            self.pages.len() as u64,
            u64::from(limits.max_pages),
        )?;
        check(
            LimitKind::Tokens,
            (self.pages.len() as u64)
                .saturating_mul(3)
                .saturating_add(12),
            u64::from(limits.max_tokens_per_object),
        )?;
        let mut updates = Vec::new();
        let mut kids = Vec::new();
        let mut retained = 0u64;
        let mut total = self.pending_bytes;
        let maximum = limits.max_file_bytes.min(u64::from(u32::MAX));
        for entry in &self.pages {
            let mut dict = self.dict_at(entry.id)?;
            explicit(&mut dict, entry);
            set(
                &mut dict,
                "Parent",
                Value::Ref(self.pages_root.number(), self.pages_root.generation()),
            );
            let body = Value::Dict(dict);
            let cost = self.cost(entry.id, &body, None)?;
            retained = value::add(
                retained,
                cost,
                LimitKind::TotalDecodedBytes,
                limits.max_total_decoded_bytes,
            )?;
            if let Some(old) = self.pending.get(&entry.id.number()) {
                total = total
                    .checked_sub(old.cost)
                    .ok_or_else(|| malformed("pending byte accounting"))?;
            }
            total = value::add(total, cost, LimitKind::FileBytes, maximum)?;
            value::add(
                self.bytes.len() as u64,
                total,
                LimitKind::FileBytes,
                maximum,
            )?;
            updates.try_reserve(1).map_err(|_| {
                refused(
                    LimitKind::TotalDecodedBytes,
                    retained.saturating_add(1),
                    retained,
                )
            })?;
            kids.try_reserve(1)
                .map_err(|_| refused(LimitKind::ObjectBytes, maximum.saturating_add(1), maximum))?;
            updates.push(Pending {
                id: entry.id,
                value: body,
                data: None,
                cost,
            });
            kids.push(Value::Ref(entry.id.number(), entry.id.generation()));
        }
        let body = Value::Dict(vec![
            ("Type".to_owned(), Value::Name("Pages".to_owned())),
            ("Kids".to_owned(), Value::Array(kids)),
            ("Count".to_owned(), Value::Int(i64::from(self.page_count()))),
        ]);
        let cost = self.cost(self.pages_root, &body, None)?;
        if let Some(old) = self.pending.get(&self.pages_root.number()) {
            total = total
                .checked_sub(old.cost)
                .ok_or_else(|| malformed("pending byte accounting"))?;
        }
        total = value::add(total, cost, LimitKind::FileBytes, maximum)?;
        value::add(
            self.bytes.len() as u64,
            total,
            LimitKind::FileBytes,
            maximum,
        )?;
        updates.push(Pending {
            id: self.pages_root,
            value: body,
            data: None,
            cost,
        });
        Ok(updates)
    }
}
