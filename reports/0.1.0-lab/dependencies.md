# 首轮实现依赖清单

生成基线：2026-09-21，来源为 `cargo metadata --locked --format-version 1` 与 `Cargo.lock`。下表包含整个解析图，含目标平台专用及构建/测试依赖，并不表示每个平台都会编译全部条目。

许可证与 MSRV 是各包元数据的声明值，未对照逐个 LICENSE 文件做分发法律审查；`未声明` 不代表兼容任意 Rust 版本。当前项目工具链固定 1.94.0。尚未完成 RustSec 公告审计，不据此宣称依赖没有漏洞。

直接依赖用途：Tokio 提供网络和异步文件；rusqlite 启用 bundled SQLite；fs2 查询磁盘空间，进程锁使用标准库；sha2 计算完整性摘要；uuid 创建内部标识；serde/serde_json/TOML 处理严格配置和 JSON 输出；clap 解析命令行；thiserror 定义错误；tempfile 用于隔离测试目录。

| 包 | 锁定版本 | 许可证声明 | MSRV 声明 |
| --- | --- | --- | --- |
| anstream | 1.0.0 | MIT OR Apache-2.0 | 1.66.0 |
| anstyle | 1.0.14 | MIT OR Apache-2.0 | 1.66.0 |
| anstyle-parse | 1.0.0 | MIT OR Apache-2.0 | 1.66.0 |
| anstyle-query | 1.1.5 | MIT OR Apache-2.0 | 1.66.0 |
| anstyle-wincon | 3.0.11 | MIT OR Apache-2.0 | 1.66.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | 1.56.0 |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 | 未声明 |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 | 1.71.1 |
| bytes | 1.12.1 | MIT | 1.57 |
| cc | 1.4.7 | MIT OR Apache-2.0 | 1.65.0 |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | 1.32 |
| clap | 4.6.7 | MIT OR Apache-2.0 | 1.85 |
| clap_builder | 4.6.7 | MIT OR Apache-2.0 | 1.85 |
| clap_derive | 4.6.7 | MIT OR Apache-2.0 | 1.85 |
| clap_lex | 1.1.1 | MIT OR Apache-2.0 | 1.85 |
| colorchoice | 1.0.5 | MIT OR Apache-2.0 | 1.66.0 |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 | 未声明 |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 | 未声明 |
| digest | 0.10.7 | MIT OR Apache-2.0 | 未声明 |
| equivalent | 1.0.2 | Apache-2.0 OR MIT | 1.6 |
| errno | 0.3.14 | MIT OR Apache-2.0 | 1.56 |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | 未声明 |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | 未声明 |
| fastrand | 2.5.0 | Apache-2.0 OR MIT | 1.63 |
| find-msvc-tools | 0.1.13 | MIT OR Apache-2.0 | 1.65.0 |
| foldhash | 0.2.0 | Zlib | 1.60 |
| fs2 | 0.4.3 | MIT/Apache-2.0 | 未声明 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | 1.36 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 | 1.71 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | 1.71 |
| generic-array | 0.14.7 | MIT | 未声明 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | 1.85 |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 | 1.65.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | 1.85.0 |
| hashlink | 0.12.2 | MIT OR Apache-2.0 | 1.85 |
| heck | 0.5.0 | MIT OR Apache-2.0 | 1.56 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT | 1.85 |
| is_terminal_polyfill | 1.70.2 | MIT OR Apache-2.0 | 1.70.0 |
| itoa | 1.0.18 | MIT OR Apache-2.0 | 1.68 |
| js-sys | 0.3.105 | MIT OR Apache-2.0 | 1.77 |
| libc | 0.2.189 | MIT OR Apache-2.0 | 1.65 |
| libsqlite3-sys | 0.38.2 | MIT | 未声明 |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 1.63 |
| memchr | 2.8.3 | Unlicense OR MIT | 1.61 |
| mio | 1.2.3 | MIT | 1.71 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | 1.65 |
| once_cell_polyfill | 1.70.2 | MIT OR Apache-2.0 | 1.70.0 |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | 1.37 |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 | 1.63 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | 1.71 |
| quote | 1.0.47 | MIT OR Apache-2.0 | 1.71 |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | 1.68 |
| rsqlite-vfs | 0.1.1 | MIT | 1.81.0 |
| rusqlite | 0.40.2 | MIT | 未声明 |
| rustix | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 1.65 |
| rustversion | 1.0.23 | MIT OR Apache-2.0 | 1.31 |
| serde | 1.0.229 | MIT OR Apache-2.0 | 1.56 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | 1.56 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | 1.71 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | 1.71 |
| serde_spanned | 1.1.1 | MIT OR Apache-2.0 | 1.85 |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | 未声明 |
| shlex | 2.0.1 | MIT OR Apache-2.0 | 1.46.0 |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 | 1.26 |
| slab | 0.4.12 | MIT | 1.51 |
| smallvec | 1.16.1 | MIT OR Apache-2.0 | 未声明 |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | 1.70 |
| sqlite-wasm-rs | 0.5.5 | MIT | 1.81.0 |
| strsim | 0.11.1 | MIT | 1.56 |
| syn | 3.0.6 | MIT OR Apache-2.0 | 1.71 |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | 1.63 |
| thiserror | 2.0.20 | MIT OR Apache-2.0 | 1.71 |
| thiserror-impl | 2.0.20 | MIT OR Apache-2.0 | 1.71 |
| tokio | 1.53.1 | MIT | 1.71 |
| tokio-macros | 2.7.2 | MIT | 1.71 |
| toml | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | 1.76 |
| toml_datetime | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | 1.76 |
| toml_parser | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | 1.85 |
| toml_writer | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | 1.85 |
| typenum | 1.20.1 | MIT OR Apache-2.0 | 1.41.0 |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | 1.71 |
| utf8parse | 0.2.2 | Apache-2.0 OR MIT | 未声明 |
| uuid | 1.26.1 | Apache-2.0 OR MIT | 1.85.0 |
| vcpkg | 0.2.15 | MIT/Apache-2.0 | 未声明 |
| version_check | 0.9.5 | MIT/Apache-2.0 | 未声明 |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 未声明 |
| wasm-bindgen | 0.2.128 | MIT OR Apache-2.0 | 1.77 |
| wasm-bindgen-macro | 0.2.128 | MIT OR Apache-2.0 | 1.77 |
| wasm-bindgen-macro-support | 0.2.128 | MIT OR Apache-2.0 | 1.77 |
| wasm-bindgen-shared | 0.2.128 | MIT OR Apache-2.0 | 1.77 |
| winapi | 0.3.9 | MIT/Apache-2.0 | 未声明 |
| winapi-i686-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 | 未声明 |
| winapi-x86_64-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 | 未声明 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | 1.71 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | 1.71 |
| winnow | 0.7.15 | MIT | 1.65.0 |
| winnow | 1.0.4 | MIT | 1.65.0 |
| zmij | 1.0.23 | MIT | 1.71 |

解析图共 98 个第三方包版本。更新依赖后需重新生成清单，并重新执行两平台检查。
