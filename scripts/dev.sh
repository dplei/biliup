#!/usr/bin/env bash
# 本机开发环境启动脚本。
#
# 用法：
#   scripts/dev.sh            # 只起后端（前端走 out/ 里已构建的静态页）
#   scripts/dev.sh --web      # 后端 + Next.js 热重载（前端改动免重新构建）
#   scripts/dev.sh --release  # 用 release 档编译后端，贴近镜像里的性能表现
#
# 后端固定读写 cwd 下的 data/data.sqlite3，所以必须从仓库根目录运行。
set -euo pipefail

cd "$(dirname "$0")/.."

# 用普通字符串而非数组：macOS 自带 bash 3.2 在 set -u 下展开空数组会直接报 unbound variable。
profile_flag=""
profile_dir=debug
web=false
for arg in "$@"; do
	case "$arg" in
		--web) web=true ;;
		--release) profile_flag=--release; profile_dir=release ;;
		*) echo "未知参数：$arg" >&2; exit 2 ;;
	esac
done

# 前端产物由 rust-embed 在 debug 档下运行时从磁盘读取，因此这里只在缺失时构建一次；
# 之后改前端跑 `npm run build` 即可生效，不用重新编译后端。
if [ ! -f out/index.html ]; then
	echo "==> out/ 不存在，先构建前端"
	npm run build
fi

cargo build -p biliup-cli --bin biliup $profile_flag

if [ "$web" = true ]; then
	# 接口地址由仓库里的 .env.development 提供（指向 localhost:19159），后端也已把
	# http://localhost:3000 加进 CORS 白名单，这里不需要再传什么。
	npm run dev &
	trap 'kill 0' EXIT
fi

# 绑 127.0.0.1 而非镜像里的 0.0.0.0：本机调试没有对外暴露的理由。
exec env RUST_LOG="${RUST_LOG:-info}" "target/$profile_dir/biliup" server --bind 127.0.0.1 --port 19159
