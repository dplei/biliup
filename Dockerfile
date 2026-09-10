# syntax=docker/dockerfile:1
#
# 三个基础镜像都钉到 digest。滚动 tag（node:lts / rust:latest / python:3.13-slim）平均每
# 6 / 16 / 13 天就会重建一次，任意一个变都会作废下游全部层缓存——1.3.23 那次就是 node:lts
# 在两次发版之间更新，把本该命中的热构建打回全量冷构建。
#
# 更新方式（想跟进上游安全补丁时手动跑，建议每月一次）：
#   docker buildx imagetools inspect node:lts --format '{{.Manifest.Digest}}'
# 拿到新 digest 换掉下面对应的一行即可；三行可以分别更新，不必同时。
# Build biliup's web-ui
FROM node:lts@sha256:6dac556d980b7f0e5498d08f08cee0ca67798b4ad6c23964a9214920e67758d0 AS webui-builder
ARG repo_url=https://github.com/biliup/biliup
ARG branch_name=master

# 依赖清单先单独进来：npm install 这层只在依赖变化时失效，不跟着源码改动重跑。
# node_modules/ 已在 .dockerignore 里，后面的 COPY . 不会覆盖这层装好的依赖。
COPY package.json package-lock.json /biliup/

RUN set -eux; \
	cd /biliup; \
	npm install;

COPY . /biliup

# 守卫仍在 WORKDIR 之前，此时 cwd 是 /，rm -rf /biliup 才安全。
# clone 模式下整个目录被换掉，上面那层依赖也就没了，得就地补装。
RUN set -eux; \
	\
	if [ ! -f /biliup/biliup.spec ]; then \
	rm -rf /biliup; \
	git clone --depth 1 --branch "$branch_name" "$repo_url" /biliup; \
	cd /biliup; \
	npm install; \
	fi;

WORKDIR /biliup

RUN set -eux; \
	npm run build;


# Build biliup's python wheel
FROM rust:latest@sha256:bf5a9aa29062a6cb03c49bd59a46eb55e3cc770caf598a221a7866e500be3082 AS wheel-builder
ARG repo_url=https://github.com/biliup/biliup
ARG branch_name=master

# 工具链与源码无关：放在 COPY 之前，源码一改不再重装整套 apt 包与 maturin。
RUN set -eux; \
	\
	apt-get update; \
	apt-get install -y --no-install-recommends python3-pip g++ patchelf; \
	pip3 install maturin --break-system-packages;

COPY . /biliup

RUN set -eux; \
	\
	if [ ! -f /biliup/biliup.spec ]; then \
	rm -rf /biliup; \
	git clone --depth 1 --branch "$branch_name" "$repo_url" /biliup; \
	fi;

COPY --from=webui-builder /biliup/out /biliup/out

WORKDIR /biliup

# cargo registry/git/target 用 BuildKit cache mount 跨次构建复用：改本地 crate 时
# 依赖不再全量重编。注意 target/ 是临时挂载、不进镜像层，必须把产物 wheel 拷到普通目录
# /wheels，否则下一阶段 COPY --from 取不到。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	--mount=type=cache,target=/biliup/target \
	set -eux; \
	rm -rf target/wheels; \
	maturin build --release; \
	mkdir -p /wheels; \
	cp target/wheels/*.whl /wheels/;


# Deploy Biliup
FROM python:3.13-slim@sha256:9d2e5553305c7c7b0097999bb17187c69b921ccd6bc9d40e4bb5ebe652c00285 AS biliup

ENV TZ="Asia/Shanghai"
ENV LANG="C.UTF-8"
ENV LANGUAGE="C.UTF-8"
ENV LC_ALL="C.UTF-8"
EXPOSE 19159/tcp
VOLUME /opt

# 系统依赖、ffmpeg 与 quickjs 都跟 wheel 无关，放在 COPY --from 之前：
# wheel 每次发版必变，排在它后面会让这一层跟着失效、每次重下 126MB 的 ffmpeg。
RUN set -eux; \
	\
	savedAptMark="$(apt-mark showmanual)"; \
	useApt=false; \
	apt-get update; \
	apt-get install -y --no-install-recommends \
		wget \
		curl \
		xz-utils \
		g++ \
	; \
	apt-mark auto '.*' > /dev/null; \
	apt-mark manual curl wget; \
	\
	arch="$(dpkg --print-architecture)"; arch="${arch##*-}"; \
	url='https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-'; \
	case "$arch" in \
		'amd64') \
			url="${url}linux64-gpl-8.1.tar.xz"; \
		;; \
		'arm64') \
			url="${url}linuxarm64-gpl-8.1.tar.xz"; \
		;; \
		*) \
			useApt=true; \
		;; \
	esac; \
	\
	if [ "$useApt" = true ] ; then \
		apt-get install -y --no-install-recommends \
			ffmpeg \
		; \
	else \
		wget -O ffmpeg.tar.xz "$url" --progress=dot:giga; \
		tar -xJf ffmpeg.tar.xz -C /usr/local --strip-components=1; \
		rm -rf \
			/usr/local/doc \
			/usr/local/man; \
		rm -rf \
			/usr/local/bin/ffplay; \
		rm -rf \
			ffmpeg*; \
		chmod a+x /usr/local/* ; \
	fi; \
	# 自动响度标准化同时依赖 ffmpeg、ffprobe 和 loudnorm；构建期直接锁住运行时能力。 \
	ffmpeg -version > /dev/null; \
	ffprobe -version > /dev/null; \
	ffmpeg -hide_banner -filters 2>/dev/null | grep -q ' loudnorm '; \
	\
	# 安装 quickjs 需要 g++
	pip3 install --no-cache-dir quickjs; \
	\
	# Clean up \
	[ -z "$savedAptMark" ] || apt-mark manual $savedAptMark; \
	apt-get purge -y --auto-remove -o APT::AutoRemove::RecommendsImportant=false; \
	rm -rf \
		/tmp/* \
		/usr/share/doc/* \
		/var/cache/* \
		/var/lib/apt/lists/* \
		/var/tmp/* \
		/var/log/* \
	;

# 需要遵守 wheel 文件名规范（产物从 cache mount 外的 /wheels 取，见 wheel-builder 注释）
COPY --from=wheel-builder /wheels/* /tmp/

RUN set -eux; \
	\
	whl=$(ls /tmp/biliup*.whl); \
	pip3 install --no-cache-dir "$whl"; \
	# pip3 install --no-cache-dir "$whl[quickjs]"; \
	pip3 cache purge; \
	rm -rf /tmp/*;

WORKDIR /opt

ENTRYPOINT ["biliup"]
