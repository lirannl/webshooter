use webshooter_macros::upon;

#[test]
fn test_upon_macro_valid_template() {
    let engine = upon!("Hello {{ name }}!");
    let template = engine.template("macro_template");
    let result = template
        .render(upon::value! { name: "World" })
        .to_string()
        .unwrap();
    assert_eq!(result, "Hello World!");
}

#[test]
fn test_upon_macro_with_conditionals() {
    let engine = upon!("{% if show %}Hello{% endif %}");
    let template = engine.template("macro_template");
    let result = template
        .render(upon::value! { show: true })
        .to_string()
        .unwrap();
    assert_eq!(result, "Hello");

    let result = template
        .render(upon::value! { show: false })
        .to_string()
        .unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_upon_macro_with_loops() {
    let engine = upon!("{% for item in items %}{{ item }}{% endfor %}");
    let template = engine.template("macro_template");
    let result = template
        .render(upon::value! { items: vec!["a", "b", "c"] })
        .to_string()
        .unwrap();
    assert_eq!(result, "abc");
}

#[test]
fn test_upon_macro_with_filters() {
    // upon doesn't have built-in filters, but we can test that the macro
    // compiles templates with filter syntax correctly
    let engine = upon!("{{ name | some_filter }}");
    let template = engine.template("macro_template");
    // This will fail at render time because the filter doesn't exist,
    // but the macro should have compiled the template successfully
    let result = template.render(upon::value! { name: "hello" }).to_string();
    assert!(result.is_err());
}

#[test]
fn test_upon_macro_with_include() {
    // Test that the macro compiles templates with include syntax correctly
    // The include feature requires the nested template to be registered at runtime
    let _engine = upon!("Hello {% include \"other\" %}");
}
