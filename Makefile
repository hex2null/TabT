# 用法:
#   make            # 编译 + 打包开发版 "dist/TabT Dev.app" + 签名
#   make run        # 打包并启动开发版
#   make release    # 打包正式版 dist/TabT.app(名称 / bundle id / 配置目录都用发布身份)
#   make dmg        # 用正式版构建安装镜像 dist/TabT-<version>.dmg
#   make cert       # 一次性创建本地稳定签名身份(见下方 CERT_NAME),让 TCC 授权跨重新编译保留
#   make echo       # 跑第 1 步的 PTY 回声环(在当前终端里)
#   make test       # tabt-core 单元测试 + tabt-app 的少量测试
#   make bloat      # 体积审计(需要 cargo install cargo-bloat)
#   make clean

# ---- Product identity ------------------------------------------------------------------
# The shipped app is "TabT"; the default build here is the *development* one, which uses a
# different app name, bundle id, executable name and config directory so it can be installed
# and run next to the released app: its own name in the Dock/menu bar, its own process name in
# `ps`, and its own ~/.tabt-dev layout file instead of overwriting the real one.
# `make release` / `make dmg` re-invoke this Makefile with the release identity.
APP_NAME   ?= TabT Dev
BUNDLE_ID  ?= dev.local.tabt.dev
CONFIG_DIR ?= .tabt-dev
EXEC       ?= tabt-dev

# Both bundles are build output, so they land in dist/ next to the disk images rather than in
# the repository root. `dist/` is hardcoded, not a variable: `bundle:` starts with
# `rm -rf "$(APP)"`, and an accidentally empty DISTDIR would point that at the filesystem root.
APP       := dist/$(APP_NAME).app
BIN       := target/release/tabt
BUNDLE    := $(APP)/Contents
CERT_NAME := TabT Dev

.PHONY: all build bundle run release dmg echo test bloat clean cert

all: bundle

# Always hand off to cargo (it does its own up-to-date check, and build.rs re-runs the crate
# when TABT_APP_NAME/TABT_CONFIG_DIR change — a file-timestamp rule would happily keep a
# binary with the other identity baked in).
build:
	TABT_APP_NAME="$(APP_NAME)" TABT_CONFIG_DIR="$(CONFIG_DIR)" cargo build --release --bin tabt

# Signing identity: prefer the stable local "TabT Dev" certificate (see `make cert`) so the
# app's designated requirement doesn't change across rebuilds and macOS TCC folder-access
# grants survive `make run`. Falls back to ad-hoc (re-prompts every rebuild) if `make cert`
# hasn't been run yet.
bundle: build
	rm -rf "$(APP)"
	mkdir -p "$(BUNDLE)/MacOS" "$(BUNDLE)/Resources"
	cp $(BIN) "$(BUNDLE)/MacOS/$(EXEC)"
	sed -e 's|__APP_NAME__|$(APP_NAME)|g' \
	    -e 's|__BUNDLE_ID__|$(BUNDLE_ID)|g' \
	    -e 's|__EXEC__|$(EXEC)|g' bundle/Info.plist.in > "$(BUNDLE)/Info.plist"
	cp bundle/AppIcon.icns "$(BUNDLE)/Resources/"
	# The shipped theme defaults (config::bundled_themes_file); copied into the user's config
	# directory on first run. Must land before codesign — a file added to Contents/Resources
	# afterwards invalidates the signature.
	cp bundle/themes.conf "$(BUNDLE)/Resources/"
	@if security find-certificate -c "$(CERT_NAME)" >/dev/null 2>&1; then \
		codesign --force --sign "$(CERT_NAME)" "$(APP)"; \
	else \
		echo "==> no '$(CERT_NAME)' signing identity found, falling back to ad-hoc (run 'make cert' once to stop TCC re-prompting on every rebuild)"; \
		codesign --force --sign - "$(APP)"; \
	fi
	@echo "==> built $(APP) ($$(du -sh "$(BUNDLE)/MacOS/$(EXEC)" | cut -f1))"

cert:
	bash bundle/make-dev-cert.sh

# killall matches the executable name, which differs per identity ($(EXEC)), so restarting the
# development build never kills a running release TabT.
run: bundle
	-killall $(EXEC) 2>/dev/null || true   # 干掉旧实例,否则 open 只会把它前置、不加载新二进制
	open "$(APP)"

release:
	$(MAKE) APP_NAME=TabT BUNDLE_ID=dev.local.tabt CONFIG_DIR=.tabt EXEC=tabt bundle

# Drag-to-Applications disk image, always built from the release identity. Set SIGN_ID /
# NOTARY_PROFILE (see bundle/make-dmg.sh) to produce one that Gatekeeper accepts on other Macs.
dmg: release
	APP=dist/TabT.app bash bundle/make-dmg.sh

# These pass the same identity as `build` so alternating with `make run` doesn't flip
# TABT_APP_NAME/TABT_CONFIG_DIR and force a full relink of tabt-app every time.
echo:
	TABT_APP_NAME="$(APP_NAME)" TABT_CONFIG_DIR="$(CONFIG_DIR)" cargo run --release --bin pty-echo

# tabt-core's tests are the platform-independent bulk; tabt-app has a few of its own (the
# themes.conf parser), which only build on macOS — same identity as `build`, to avoid a relink.
test:
	cargo test -p tabt-core
	TABT_APP_NAME="$(APP_NAME)" TABT_CONFIG_DIR="$(CONFIG_DIR)" cargo test -p tabt-app

bloat:
	TABT_APP_NAME="$(APP_NAME)" TABT_CONFIG_DIR="$(CONFIG_DIR)" cargo bloat --release --bin tabt -n 20

# Removes both bundles but keeps dist/*.dmg: the images are the shippable output, and rebuilding
# one means re-running the whole release+notarization path.
clean:
	cargo clean
	rm -rf "$(APP)" dist/TabT.app
