use std::sync::Arc;

use pdf_core::{DocError, LimitKind, ObjectRef};
use pdf_write::annot::{Annot, AnnotKind};
use pdf_write::Value;

use crate::{check, get, malformed, set, Editor, Pending, StreamData};

impl Editor {
    pub fn add_annotation(&mut self, page: u32, annot: &Annot) -> Result<ObjectRef, DocError> {
        self.clean()?;
        let page_id = self.page_ref(page)?;
        let limits = self.document.limits();
        let mut numbers = 12u64;
        let mut text = annot.contents.as_ref().map_or(0, |text| text.len() as u64)
            .saturating_add(annot.author.as_ref().map_or(0, |text| text.len() as u64));
        match &annot.kind {
            AnnotKind::Highlight { quads } => {
                numbers = numbers.saturating_add((quads.len() as u64).saturating_mul(8));
            }
            AnnotKind::Ink { paths, .. } => {
                numbers = numbers.saturating_add(paths.len() as u64);
                for path in paths {
                    numbers = numbers.saturating_add((path.len() as u64).saturating_mul(2));
                }
            }
            AnnotKind::FreeText { text: contents, .. } => {
                text = text.saturating_add(contents.len() as u64);
            }
            _ => {}
        }
        check(
            LimitKind::Tokens,
            numbers.saturating_mul(4).saturating_add(128),
            u64::from(limits.max_tokens_per_object),
        )?;
        let estimate = numbers.saturating_mul(128)
            .saturating_add(text.saturating_mul(128)).saturating_add(8192);
        check(LimitKind::ObjectBytes, estimate, limits.max_object_bytes)?;
        check(LimitKind::DecodedBytes, estimate, limits.max_decoded_bytes)?;
        let after = self.fresh(2)?;
        let ap_id = ObjectRef::new(self.next, 0);
        let id = ObjectRef::new(self.next.checked_add(1)
            .ok_or_else(|| malformed("annotation object number"))?, 0);
        let mut parts = pdf_write::annot::build(annot).map_err(malformed)?;
        let length = i64::try_from(parts.ap_content.len())
            .map_err(|_| malformed("appearance stream Length"))?;
        set(&mut parts.ap_dict, "Length", Value::Int(length));
        set(&mut parts.dict, "P", Value::Ref(page_id.number(), page_id.generation()));
        set(&mut parts.dict, "AP", Value::Dict(vec![
            ("N".to_owned(), Value::Ref(ap_id.number(), ap_id.generation())),
        ]));
        let mut dict = self.dict_at(page_id)?;
        let slot = get(&dict, "Annots").cloned().unwrap_or(Value::Null);
        let mut annots = self.array_slot(slot, false)?;
        annots.push(Value::Ref(id.number(), id.generation()));
        set(&mut dict, "Annots", Value::Array(annots));
        let appearance = Value::Dict(parts.ap_dict);
        let annotation = Value::Dict(parts.dict);
        let page_body = Value::Dict(dict);
        let ap_cost = self.cost(ap_id, &appearance, Some(&parts.ap_content))?;
        let cost = self.cost(id, &annotation, None)?;
        let page_cost = self.cost(page_id, &page_body, None)?;
        let total = self.changed_size(&[
            (ap_id, ap_cost), (id, cost), (page_id, page_cost),
        ])?;
        self.install(Pending {
            id: ap_id,
            value: appearance,
            data: Some(StreamData::owned(Arc::from(parts.ap_content))),
            cost: ap_cost,
        });
        self.install(Pending { id, value: annotation, data: None, cost });
        self.install(Pending {
            id: page_id, value: page_body, data: None, cost: page_cost,
        });
        self.next = after;
        self.pending_bytes = total;
        Ok(id)
    }

    pub fn remove_annotation(&mut self, page: u32, index: usize) -> Result<(), DocError> {
        self.clean()?;
        let id = self.page_ref(page)?;
        let mut dict = self.dict_at(id)?;
        let slot = get(&dict, "Annots").cloned().unwrap_or(Value::Null);
        let mut annots = self.array_slot(slot, false)?;
        if index >= annots.len() {
            return Err(malformed("annotation index"));
        }
        annots.remove(index);
        set(&mut dict, "Annots", Value::Array(annots));
        let body = Value::Dict(dict);
        let cost = self.cost(id, &body, None)?;
        let total = self.changed_size(&[(id, cost)])?;
        self.install(Pending { id, value: body, data: None, cost });
        self.pending_bytes = total;
        Ok(())
    }
}
