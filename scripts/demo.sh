#!/usr/bin/env bash

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER_BIN="${PROJECT_DIR}/target/release/rkv-server"
CLIENT_BIN="${PROJECT_DIR}/target/release/rkv-client"
DEMO_PORT="${RKV_DEMO_PORT:-17878}"
DEMO_ADDR="127.0.0.1:${DEMO_PORT}"
DEMO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rkv-demo.XXXXXX")"
DATA_FILE="${DEMO_DIR}/rkv.log"
SERVER_LOG="${DEMO_DIR}/server.log"
SERVER_PID=""

cleanup() {
    stop_server
    if [[ "${RKV_DEMO_KEEP:-0}" == "1" ]]; then
        echo "演示数据已保留在：${DEMO_DIR}"
    elif [[ -n "${DEMO_DIR}" && -d "${DEMO_DIR}" ]]; then
        rm -rf -- "${DEMO_DIR}"
    fi
}

stop_server() {
    if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}"
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    SERVER_PID=""
}

heading() {
    echo
    echo "============================================================"
    echo "$1"
    echo "============================================================"
}

start_server() {
    : >"${SERVER_LOG}"
    "${SERVER_BIN}" --addr "${DEMO_ADDR}" --data "${DATA_FILE}" >"${SERVER_LOG}" 2>&1 &
    SERVER_PID=$!

    local ready=0
    for _ in {1..50}; do
        if grep -q "启动成功" "${SERVER_LOG}"; then
            ready=1
            break
        fi
        if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
            echo "服务器启动失败："
            sed -n '1,80p' "${SERVER_LOG}"
            exit 1
        fi
        sleep 0.1
    done

    if [[ "${ready}" != "1" ]]; then
        echo "等待服务器启动超时："
        sed -n '1,80p' "${SERVER_LOG}"
        exit 1
    fi
    sed -n '1,5p' "${SERVER_LOG}"
}

run_client() {
    printf '%s\n' "$@" | "${CLIENT_BIN}" --addr "${DEMO_ADDR}"
}

trap cleanup EXIT INT TERM

heading "0/5 构建 release 版本"
cargo build --release --manifest-path "${PROJECT_DIR}/Cargo.toml"

heading "1/5 启动服务器并演示 CRUD、排序和错误隔离"
start_server
run_client \
    "PING" \
    "SET course Rust-programming" \
    "SET student 张三" \
    "GET course" \
    "LIST" \
    "SET only-key" \
    "PING" \
    "QUIT"

heading "2/5 四个客户端并发写入 20 个键"
worker_pids=()
for worker in {1..4}; do
    (
        for item in {1..5}; do
            printf 'SET worker_%s_%s value_%s_%s\n' "${worker}" "${item}" "${worker}" "${item}"
        done
        printf 'QUIT\n'
    ) | "${CLIENT_BIN}" --addr "${DEMO_ADDR}" >"${DEMO_DIR}/worker-${worker}.log" &
    worker_pids+=("$!")
done
for worker_pid in "${worker_pids[@]}"; do
    wait "${worker_pid}"
done
run_client "GET worker_3_4" "STATS" "QUIT"

heading "3/5 TTL：写入 2 秒后自动过期的键"
run_client "SETEX code 2 123456" "GET code" "QUIT"
echo "等待 3 秒……"
sleep 3
run_client "GET code" "PING" "QUIT"

heading "4/5 压缩日志并使用同一数据文件重启"
run_client "COMPACT" "GET course" "QUIT"
echo "停止旧服务器……"
stop_server
echo "使用同一数据文件重新启动……"
start_server

heading "5/5 验证永久数据保留、TTL 数据消失"
run_client "GET course" "GET student" "GET code" "GET worker_3_4" "STATS" "QUIT"

heading "演示完成"
echo "已覆盖：协议、CRUD、多客户端并发、TTL、异常隔离、日志压缩和重启恢复。"
