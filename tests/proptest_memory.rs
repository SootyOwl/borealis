//! Discovery-only models: generated IDs are handles, never oracle input.
use borealis::memory::{Memory, SqliteMemory};
use proptest::prelude::*;
use rusqlite::Connection;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
enum Op {
    Create(u8, Vec<u8>),
    Update(u8, u8),
    Tag(u8, Vec<u8>),
    Delete(u8),
    Link(u8, u8, u8),
    InvalidTag(u8),
    Reopen,
    Observe,
}
fn ops() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..3, proptest::collection::vec(0u8..5,0..4)).prop_map(|(v,t)|Op::Create(v,t)),
        3 => (any::<u8>(),0u8..3).prop_map(|(n,v)|Op::Update(n,v)),
        3 => (any::<u8>(),proptest::collection::vec(0u8..5,0..4)).prop_map(|(n,t)|Op::Tag(n,t)),
        1 => any::<u8>().prop_map(Op::Delete),
        3 => (any::<u8>(),any::<u8>(),0u8..3).prop_map(|(a,b,r)|Op::Link(a,b,r)),
        1 => any::<u8>().prop_map(Op::InvalidTag),
        1 => Just(Op::Reopen), 2 => Just(Op::Observe),
    ]
}
const WORDS: [&str; 3] = ["quartz", "zebra", "violet"];
const TAGS: [&str; 5] = [
    "topic",
    "topic/child",
    "topic/other",
    "topic_extra",
    "separate",
];
#[derive(Debug)]
struct ModelNote {
    id: String,
    value: u8,
    tags: BTreeSet<String>,
    live: bool,
}
fn tags(raw: &[u8]) -> Vec<String> {
    raw.iter().map(|x| TAGS[*x as usize].to_string()).collect()
}
fn ids<T: IntoIterator<Item = String>>(items: T) -> BTreeSet<String> {
    let items = items.into_iter().collect::<Vec<_>>();
    let unique = items.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), items.len(), "duplicate result IDs");
    unique
}
fn check(store: &SqliteMemory, model: &[ModelNote], links: &BTreeMap<(usize, usize), u8>) {
    let all = store.list_notes(None).unwrap();
    assert_eq!(
        ids(all.into_iter().map(|n| n.id)),
        ids(model.iter().filter(|n| n.live).map(|n| n.id.clone()))
    );
    for (i, n) in model.iter().enumerate() {
        let actual = store.read_note(&n.id);
        if !n.live {
            assert!(actual.is_err());
            continue;
        }
        let actual = actual.unwrap();
        assert_eq!(actual.content, WORDS[n.value as usize]);
        assert_eq!(actual.tags.into_iter().collect::<BTreeSet<_>>(), n.tags);
        let mut expected = Vec::new();
        for (&(a, b), &r) in links {
            if a == i {
                expected.push((
                    model[a].id.clone(),
                    model[b].id.clone(),
                    r.to_string(),
                    "outgoing".to_string(),
                ));
            }
            if b == i {
                expected.push((
                    model[a].id.clone(),
                    model[b].id.clone(),
                    r.to_string(),
                    "incoming".to_string(),
                ));
            }
        }
        let mut got = actual
            .links
            .into_iter()
            .map(|l| (l.from_id, l.to_id, l.relation, l.direction.unwrap()))
            .collect::<Vec<_>>();
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }
    for (v, word) in WORDS.iter().enumerate() {
        let got = store.search_notes(word, usize::MAX, None, false).unwrap();
        assert_eq!(
            ids(got.into_iter().map(|n| n.id)),
            ids(model
                .iter()
                .filter(|n| n.live && n.value as usize == v)
                .map(|n| n.id.clone()))
        );
    }
    for filter in [None, Some("topic"), Some("topic_extra")] {
        for exact in [false, true] {
            let expected = ids(model
                .iter()
                .filter(|n| {
                    n.live
                        && filter.map_or(true, |tag| {
                            n.tags
                                .iter()
                                .any(|t| t == tag || (!exact && t.starts_with(&format!("{tag}/"))))
                        })
                })
                .map(|n| n.id.clone()));
            let page = store
                .list_notes_paginated(filter, exact, usize::MAX, 0)
                .unwrap();
            assert_eq!(page.total, expected.len());
            assert_eq!(ids(page.notes.into_iter().map(|n| n.id)), expected);
        }
    }
    let expected = model
        .iter()
        .filter(|n| n.live)
        .flat_map(|n| n.tags.iter())
        .fold(BTreeMap::<String, usize>::new(), |mut counts, t| {
            *counts.entry(t.clone()).or_default() += 1;
            counts
        });
    let actual = store
        .list_tags(None)
        .unwrap()
        .into_iter()
        .map(|t| (t.tag, t.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual, expected);
}
proptest! {
    // PROPTEST_CASES can increase the search budget without changing generators.
    #[test]
    fn memory_histories_match_model(history in proptest::collection::vec(ops(),1..70)) {
        let conn=Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let mut store=SqliteMemory::new(conn.clone(),"unused-core".into()).unwrap();
        let mut model=Vec::<ModelNote>::new(); let mut links=BTreeMap::new();
        let mut coverage=[0usize;8];
        for op in history {
            let model_len=model.len();
            match op {
                Op::Create(v,t)=>{ coverage[0]+=1; let actual=store.create_note("Entry",WORDS[v as usize],&tags(&t)).unwrap();model.push(ModelNote{id:actual.id,value:v,tags:tags(&t).into_iter().collect(),live:true}); },
                Op::Reopen=>{coverage[5]+=1;store=SqliteMemory::new(conn.clone(),"unused-core".into()).unwrap();},
                Op::Observe=>check(&store,&model,&links),
                _ if model.is_empty()=>{assert!(store.read_note("missing").is_err());},
                Op::Update(i,v)=>{let n=&mut model[i as usize%model_len]; let result=store.update_note(&n.id,WORDS[v as usize]); assert_eq!(result.is_ok(),n.live);if n.live {coverage[1]+=1;n.value=v;}},
                Op::Tag(i,t)=>{let n=&mut model[i as usize%model_len];let result=store.tag_note(&n.id,&tags(&t));assert_eq!(result.is_ok(),n.live);if n.live {coverage[2]+=1;n.tags=tags(&t).into_iter().collect();}},
                Op::Delete(i)=>{let n=&mut model[i as usize%model_len];assert_eq!(store.forget_note(&n.id).is_ok(),n.live);coverage[3]+=usize::from(n.live);n.live=false;},
                Op::Link(a,b,r)=>{let a=a as usize%model.len();let b=b as usize%model.len();let ok=model[a].live && model[b].live;assert_eq!(store.link_notes(&model[a].id,&model[b].id,&r.to_string()).is_ok(),ok);if ok {coverage[4]+=1;coverage[7]+=usize::from(links.insert((a,b),r).is_some());}},
                Op::InvalidTag(i)=>{coverage[6]+=1;let n=&model[i as usize%model_len];assert!(store.tag_note(&n.id,&["bad tag".into()]).is_err());},
            }
        }
        check(&store,&model,&links);
        eprintln!("memory_behavioral_coverage [create, update, retag, delete, link, reopen, invalid_tag, overwrite]: {coverage:?}");
    }
    #[test]
    fn pagination_large_offsets_obey_slice_semantics(offset in prop_oneof![0usize..5, Just(i64::MAX as usize), Just(usize::MAX), any::<usize>()], limit in prop_oneof![0usize..5,Just(usize::MAX)], exact in any::<bool>(), filtered in any::<bool>()) {
        let store=SqliteMemory::new(Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),"unused-core".into()).unwrap();
        store.create_note("Entry","quartz",&["topic".into()]).unwrap();
        let page=store.list_notes_paginated(filtered.then_some("topic"),exact,limit,offset).unwrap();
        prop_assert_eq!(page.total,1);
        prop_assert_eq!(page.notes.len(),usize::from(offset==0 && limit>0));
        prop_assert_eq!(page.offset,offset);prop_assert_eq!(page.limit,limit);
    }
}
