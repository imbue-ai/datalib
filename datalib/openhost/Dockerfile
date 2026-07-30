# Datalib — Open Host container (v0 stub)
# Builds the axum HTTP backend and bakes in the static UI bundle.

FROM rust:1.83 AS backend-build
WORKDIR /src
COPY datalib/backend ./datalib/backend
COPY schemas ./schemas
WORKDIR /src/datalib/backend
RUN cargo build --release -p datalib-http

FROM node:20 AS ui-build
WORKDIR /src/datalib/ui
COPY datalib/ui/package.json datalib/ui/pnpm-lock.yaml ./
RUN corepack enable && pnpm install --frozen-lockfile
COPY datalib/ui ./
RUN pnpm build

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=backend-build /src/datalib/backend/target/release/datalib-http /app/datalib-http
COPY --from=ui-build /src/datalib/ui/dist /app/dist
EXPOSE 8731
CMD ["/app/datalib-http"]
