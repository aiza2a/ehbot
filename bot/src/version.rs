pub(crate) static VERSION: &str = concat!(
    "\n",
    "构建时间戳：\t",
    env!("VERGEN_BUILD_TIMESTAMP"),
    "\n",
    "包版本：\t",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Rustc版本：  \t",
    env!("VERGEN_RUSTC_SEMVER"),
    "\n",
    "Cargo目标平台：   \t",
    env!("VERGEN_CARGO_TARGET_TRIPLE"),
    "\n"，
    "源碼:    \thttps://github.com/qini7-sese/eh2telegraph"
);
