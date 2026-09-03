use std::path::{Path, PathBuf};

const UI_FILES: [&str; 4] = [
    "ui/main.slint",
    "ui/dialogs/about.slint",
    "ui/dialogs/ffmpeg_not_found.slint",
    "ui/dialogs/message.slint",
];

fn compile_slint(
    manifest_dir: &Path,
    out_dir: &Path,
    relative_path: &str,
    include_paths: Vec<PathBuf>,
) {
    let input = manifest_dir.join(relative_path);

    let mut diag = i_slint_compiler::diagnostics::BuildDiagnostics::default();
    let syntax_node = i_slint_compiler::parser::parse_file(&input, &mut diag);
    if diag.has_errors() {
        diag.print();
        panic!("failed to parse {relative_path}");
    }

    let mut config = i_slint_compiler::CompilerConfiguration::new(
        i_slint_compiler::generator::OutputFormat::Rust,
    );
    // 自定义 fluent-lite 风格：复制官方 fluent 组件并改 FluentPalette 主色为浅蓝 #42A5F5。
    // include_paths 需要两个条目：风格目录本身（让 ui/*.slint 的 `std-widgets.slint`
    // 裸导入解析到自定义风格），以及其父目录（让编译器把 "fluent-lite" 识别为已知风格）。
    config.style = Some("fluent-lite".into());
    config.include_paths = include_paths;
    config.translation_domain = Some(env!("CARGO_PKG_NAME").to_string());
    // 文件系统加载的风格文件不是 builtin（expose_internal_types=false），访问不了
    // TabWidget 等内部类型与 SlintInternal，也用不了 experimental interface。
    // 开启 enable_experimental 后对所有文档放开这些能力（对应用自身的标准 .slint 无害）。
    config.enable_experimental = true;

    let syntax_node = syntax_node.expect("diags contained no compilation errors");
    // 'spin_on' 与 slint-build 一致：编译器单线程、不会阻塞在没有 future 上
    let (doc, diag, loader) =
        spin_on::spin_on(i_slint_compiler::compile_syntax_node(syntax_node, diag, config));

    if diag.has_errors()
        || (!diag.is_empty() && std::env::var("SLINT_COMPILER_DENY_WARNINGS").is_ok())
    {
        diag.print();
        panic!("failed to compile {relative_path}");
    }

    // 先收集依赖（diagnostics_as_string 会消费 diag）
    for dep in &diag.all_loaded_files {
        if dep.is_absolute() {
            println!("cargo:rerun-if-changed={}", dep.display());
        }
    }
    println!("cargo:rerun-if-changed={}", input.display());

    // 警告以 cargo:warning 透传给终端（与 slint-build 一致）
    diag.diagnostics_as_string().lines().for_each(|w| {
        if !w.is_empty() {
            println!("cargo:warning={}", w.strip_prefix("warning: ").unwrap_or(w))
        }
    });

    let generated = i_slint_compiler::generator::rust::generate(&doc, &loader.compiler_config)
        .expect("failed to generate rust code");
    let stem = Path::new(relative_path).file_stem().expect("slint file has no stem");
    let out_file = out_dir.join(stem).with_extension("rs");
    std::fs::write(&out_file, generated.to_string()).expect("failed to write generated code");
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let third_party = manifest_dir.join("third_party");
    // 顺序：风格目录在前（保证 `std-widgets.slint` 裸导入命中），父目录在后（风格校验）。
    let include_paths = vec![third_party.join("fluent-lite"), third_party];

    for f in UI_FILES {
        compile_slint(&manifest_dir, &out_dir, f, include_paths.clone());
    }

    // 把 assets/app_icon.ico 作为 Windows 主应用图标嵌入 exe。
    // 只在 Windows 目标上执行；非 Windows 构建直接跳过。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app_icon.rc");
        println!("cargo:rerun-if-changed=assets/app_icon.ico");
        embed_resource::compile("assets/app_icon.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed app icon");
    }
}
