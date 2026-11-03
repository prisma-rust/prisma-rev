//! Integration tests for prism-reflect: derive, dynamic registry, and RTTIBin
//! round-trips.

use prism_reflect::{rtti_bin, Reflect, ReflectError, TypeId, TypeRegistry, Value};

#[derive(Debug, Default, Clone, PartialEq, Reflect)]
struct Inner {
    a: f32,
    b: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Reflect)]
#[reflect(name = "test::Outer", version = 3)]
struct Outer {
    name: String,
    pos: glam::Vec3,
    items: Vec<f32>,
    maybe: Option<String>,
    inner: Inner,
    #[reflect(skip)]
    runtime_only: u64,
    #[reflect(read_only)]
    id: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Reflect)]
struct Tuple(pub bool, pub String);

fn sample() -> Outer {
    Outer {
        name: "hello".into(),
        pos: glam::Vec3::new(1.0, 2.0, 3.0),
        items: vec![1.5, 2.5],
        maybe: Some("opt".into()),
        inner: Inner { a: 9.0, b: true },
        runtime_only: 0,
        id: 7,
    }
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new();
    registry.register::<Outer>();
    registry.register::<Inner>();
    registry.register::<Tuple>();
    registry.register::<glam::Vec3>();
    registry
}

#[test]
fn type_id_is_stable_and_const() {
    const A: TypeId = TypeId::from_name("test::Outer");
    const B: TypeId = TypeId::from_name("test::Outer");
    const C: TypeId = TypeId::from_name("test::Other");
    assert_eq!(A, B);
    assert_ne!(A, C);
    assert_eq!(A, TypeId::from_name("test::Outer"));
}

#[test]
fn descriptor_has_fields_and_offsets() {
    let desc = Outer::type_desc();
    assert_eq!(desc.name, "test::Outer");
    assert_eq!(desc.version, 3);
    assert_eq!(desc.size, std::mem::size_of::<Outer>());
    assert!(desc.field("pos").is_some());
    assert!(desc.field("runtime_only").is_none());
    assert!(desc.field("id").unwrap().read_only);
    assert!(desc
        .field("inner")
        .unwrap()
        .ty
        .display_name()
        .starts_with("struct"));
}

#[test]
fn get_and_set_fields() {
    let mut value = sample();

    assert_eq!(value.get_field("name"), Some(Value::Str("hello".into())));
    value.set_field("name", Value::Str("world".into())).unwrap();
    assert_eq!(value.name, "world");

    value
        .set_field("items", Value::List(vec![Value::F64(4.0)]))
        .unwrap();
    assert_eq!(value.items, vec![4.0]);

    value.set_field("maybe", Value::None).unwrap();
    assert_eq!(value.maybe, None);

    let inner = Value::Object(Inner { a: 1.0, b: false }.to_object());
    value.set_field("inner", inner).unwrap();
    assert_eq!(value.inner, Inner { a: 1.0, b: false });

    let pos = Value::Object(glam::Vec3::new(4.0, 5.0, 6.0).to_object());
    value.set_field("pos", pos).unwrap();
    assert_eq!(value.pos, glam::Vec3::new(4.0, 5.0, 6.0));
}

#[test]
fn set_rejects_unknown_and_read_only() {
    let mut value = sample();
    assert_eq!(
        value.set_field("nope", Value::None),
        Err(ReflectError::UnknownField("nope".into()))
    );
    assert_eq!(
        value.set_field("id", Value::U64(1)),
        Err(ReflectError::ReadOnly("id".into()))
    );
    // The deserialization path ignores read_only.
    value.set_field_force("id", Value::U64(99)).unwrap();
    assert_eq!(value.id, 99);
}

#[test]
fn from_object_keeps_defaults_for_missing_and_skips_unknown() {
    let mut object = sample().to_object();
    object.fields.remove("name");
    object
        .fields
        .insert("not_in_schema".into(), Value::Bool(true));

    let restored = Outer::from_object(&object).unwrap();
    assert_eq!(restored.name, ""); // Default kept
    assert_eq!(restored.pos, glam::Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(restored.id, 7); // read_only field restored via the force path
                                // skip fields are never restored from the object.
    assert_eq!(restored.runtime_only, 0);
}

#[test]
fn from_object_checks_type_id() {
    let object = Inner { a: 1.0, b: true }.to_object();
    let err = Outer::from_object(&object).unwrap_err();
    assert!(matches!(err, ReflectError::TypeIdMismatch { .. }));
}

#[test]
fn tuple_struct_uses_indices_as_names() {
    let value = Tuple(true, "x".into());
    let object = value.to_object();
    assert_eq!(object.get("0"), Some(&Value::Bool(true)));
    assert_eq!(object.get("1"), Some(&Value::Str("x".into())));

    let restored = Tuple::from_object(&object).unwrap();
    assert_eq!(restored, value);
}

#[test]
fn registry_restores_without_static_type() {
    let registry = registry();
    let object = sample().to_object();

    let any = registry.from_object(&object).unwrap();
    let restored = any.downcast_ref::<Outer>().expect("downcast Outer");
    assert_eq!(restored, &sample());

    // Reverse direction: Any -> Object.
    let any: Box<dyn std::any::Any> = Box::new(sample());
    let object2 = registry.to_object(any.as_ref()).expect("registered");
    assert_eq!(object, object2);

    assert!(registry.descriptor_by_name("test::Outer").is_some());
    assert!(registry
        .descriptor(TypeId::from_name("test::Outer"))
        .is_some());
    assert_eq!(registry.len(), 4);
}

/// Two different Rust types must not share one reflect name (#166): the
/// stable `TypeId` is derived only from the name, so a silent overwrite
/// would let data written by the first type be decoded by the wrong type.
#[derive(Debug, Default, Clone, PartialEq, Reflect)]
#[reflect(name = "test::Clash")]
struct ClashA {
    a: f32,
}

#[derive(Debug, Default, Clone, PartialEq, Reflect)]
#[reflect(name = "test::Clash")]
struct ClashB {
    b: u32,
}

#[test]
#[should_panic(expected = "is already registered by")]
fn registry_rejects_a_second_rust_type_with_the_same_reflect_name() {
    let mut registry = TypeRegistry::new();
    registry.register::<ClashA>();
    registry.register::<ClashB>();
}

#[test]
fn registry_accepts_the_same_type_twice() {
    let mut registry = TypeRegistry::new();
    assert_eq!(registry.register::<Outer>(), None);
    assert_eq!(registry.register::<Outer>(), Some(Outer::type_desc().id));
    assert_eq!(registry.len(), 1);

    // Re-registering must not break the restore path.
    let any = registry.from_object(&sample().to_object()).unwrap();
    assert_eq!(any.downcast_ref::<Outer>().unwrap(), &sample());
}

#[test]
fn rtti_bin_round_trip_and_determinism() {
    let registry = registry();
    let object = sample().to_object();

    let bytes = rtti_bin::encode_one(&object, &registry).unwrap();
    assert_eq!(&bytes[..4], &rtti_bin::MAGIC);

    // Encoding the same input twice must produce identical bytes.
    let bytes2 = rtti_bin::encode_one(&object, &registry).unwrap();
    assert_eq!(bytes, bytes2);

    let decoded = rtti_bin::decode(&bytes).unwrap();
    assert_eq!(decoded.objects, vec![object.clone()]);
    assert_eq!(decoded.type_table.len(), 3); // Outer + Inner + glam::Vec3
    assert_eq!(
        decoded
            .type_table
            .get_by_name("test::Outer")
            .unwrap()
            .version,
        3
    );

    // Restore the decoded result to a concrete type via the registry.
    let any = registry.from_object(&decoded.objects[0]).unwrap();
    assert_eq!(any.downcast_ref::<Outer>().unwrap(), &sample());

    // Nested objects are present in the type table.
    assert!(decoded.type_table.get(Inner::type_desc().id).is_some());
}

#[test]
fn rtti_bin_multiple_objects() {
    let registry = registry();
    let objects = vec![sample().to_object(), Inner { a: 2.0, b: false }.to_object()];

    let bytes = rtti_bin::encode(&objects, &registry).unwrap();
    let decoded = rtti_bin::decode(&bytes).unwrap();
    assert_eq!(decoded.objects, objects);
}

#[test]
fn rtti_bin_requires_registered_types() {
    let registry = TypeRegistry::new();
    let err = rtti_bin::encode_one(&sample().to_object(), &registry).unwrap_err();
    assert!(matches!(err, rtti_bin::RttiError::NotRegistered(_)));
}

#[test]
fn rtti_bin_rejects_bad_input() {
    assert_eq!(
        rtti_bin::decode(b"nope").unwrap_err(),
        rtti_bin::RttiError::BadMagic
    );

    let registry = registry();
    let mut bytes = rtti_bin::encode_one(&sample().to_object(), &registry).unwrap();
    bytes.push(0);
    assert_eq!(
        rtti_bin::decode(&bytes).unwrap_err(),
        rtti_bin::RttiError::TrailingBytes
    );

    // Truncated input.
    let bytes = rtti_bin::encode_one(&sample().to_object(), &registry).unwrap();
    let truncated = &bytes[..bytes.len() / 2];
    assert!(rtti_bin::decode(truncated).is_err());
}

#[test]
fn value_type_errors_are_reported() {
    let mut value = sample();
    let err = value
        .set_field("items", Value::Str("x".into()))
        .unwrap_err();
    assert!(matches!(err, ReflectError::TypeMismatch { .. }));

    let err = value.set_field("inner", Value::F64(1.0)).unwrap_err();
    assert!(matches!(err, ReflectError::TypeMismatch { .. }));
}

/// Container types should forward the inner enum's variant names to the schema
/// (the editor dropdown relies on it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Flag {
    #[default]
    Off,
    On,
}

impl prism_reflect::ReflectType for Flag {
    fn field_type() -> prism_reflect::FieldType {
        prism_reflect::FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Off", "On"])
    }

    fn to_value(&self) -> Value {
        Value::U64(u64::from(*self as u8))
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Off),
            1 => Ok(Self::On),
            other => Err(ReflectError::BadData(format!("invalid Flag: {other}"))),
        }
    }
}

#[test]
fn containers_forward_enum_variants() {
    use prism_reflect::ReflectType;

    let expected = Some(&["Off", "On"][..]);
    assert_eq!(Flag::enum_variants(), expected);
    assert_eq!(Vec::<Flag>::enum_variants(), expected);
    assert_eq!(Option::<Flag>::enum_variants(), expected);
    assert_eq!(<[Flag; 4]>::enum_variants(), expected);

    // Non-enum fields are still None (an absent enum must not be confused with
    // an empty variant list).
    assert_eq!(u8::enum_variants(), None);
    assert_eq!(Vec::<u8>::enum_variants(), None);
    assert_eq!(Option::<String>::enum_variants(), None);
}
