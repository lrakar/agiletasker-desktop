#!/usr/bin/env node
/**
 * prepare-deps — the two things `npx tauri build`/`tauri dev` need that
 * aren't source code: a Node runtime to embed as the sidecar binary
 * (`bundle.externalBin`), and the native `node_modules` (node-pty, werift)
 * the workspace-host daemon needs physically adjacent to it at runtime
 * (`bundle.resources` copies `resources/daemon-deps/` in — see
 * `daemon::bundles::ensure_daemon_deps` on the Rust side for how it lands
 * in `app_data` from there).
 *
 * (a) Downloads the official Node runtime for the current platform from
 *     nodejs.org/dist, verifies the extracted binary actually runs
 *     (`--version`), and places it at the target-triple-suffixed path Tauri
 *     expects for a sidecar.
 * (b) `npm install --omit=dev node-pty@^1.1.0 werift@^0.24.3` in a scratch
 *     staging dir (outside the repo tree — this script's own output is
 *     never something to commit) and copies the resulting `node_modules`
 *     into `src-tauri/resources/daemon-deps/node_modules`.
 *
 * Idempotent: re-running with existing output does nothing. `--force`
 * wipes and redoes both steps regardless.
 *
 * Target defaults to the CURRENT host (right for local dev and a same-arch
 * CI runner). Override for a cross-compile leg — e.g. desktop/ci/release.yml
 * builds x86_64-apple-darwin on an arm64 runner via `tauri build --target
 * x86_64-apple-darwin` — with:
 *   PREP_TARGET_TRIPLE=x86_64-apple-darwin   (required for cross-compiling)
 *   PREP_NODE_ARCH=x64                       (optional — inferred from the
 *                                              triple's arch component when
 *                                              omitted; x86_64->x64,
 *                                              aarch64->arm64)
 * When cross-compiling, the extracted binary's `--version` self-test is
 * skipped (the host generally can't execute a foreign-arch binary without
 * something like Rosetta 2) in favor of a byte-size sanity check.
 *
 * Usage: node scripts/prepare-deps.mjs [--force]
 */

import { execFileSync } from 'node:child_process'
import { existsSync, mkdtempSync, rmSync, cpSync, writeFileSync, mkdirSync, statSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const DESKTOP_ROOT = path.resolve(__dirname, '..')
const SRC_TAURI = path.join(DESKTOP_ROOT, 'src-tauri')

// Pinned exactly to the version verified working on the dev machine this
// task was built on (`node --version` -> v24.13.0) — a specific pin, not
// "whatever nodejs.org calls latest today", so every build (and every CI
// runner) embeds the identical, already-tested runtime.
const NODE_VERSION = '24.13.0'
const DAEMON_DEPS = ['node-pty@^1.1.0', 'werift@^0.24.3']

const FORCE = process.argv.includes('--force')

function log(msg) {
  console.log(`[prepare-deps] ${msg}`)
}

// --- platform/arch -> nodejs.org dist name + Rust target triple -------------

const PLATFORM_DIST = { win32: 'win', darwin: 'darwin', linux: 'linux' }
const PLATFORM_RUST_OS = { win32: 'pc-windows-msvc', darwin: 'apple-darwin', linux: 'unknown-linux-gnu' }
const ARCH_DIST = { x64: 'x64', arm64: 'arm64' }
const ARCH_RUST = { x64: 'x86_64', arm64: 'aarch64' }
// Reverse of ARCH_RUST, for turning a target triple's arch component back
// into a Node dist arch when PREP_NODE_ARCH itself isn't given alongside
// PREP_TARGET_TRIPLE.
const RUST_ARCH_TO_DIST = { x86_64: 'x64', aarch64: 'arm64' }

/**
 * Host auto-detection (process.platform/process.arch) is right for local
 * dev and a same-arch CI runner, but wrong for a CROSS-compile leg — e.g.
 * this project's release workflow (desktop/ci/release.yml) builds the
 * x86_64-apple-darwin leg on an arm64 (Apple Silicon) runner via `tauri
 * build --target x86_64-apple-darwin`. Tauri's sidecar resolution keys off
 * the ACTIVE BUILD TARGET, not the host, so the embedded node binary must
 * be named/fetched for that target too. `PREP_TARGET_TRIPLE` (and
 * optionally `PREP_NODE_ARCH`, when the triple's arch component alone is
 * ambiguous or the caller just wants to be explicit) override
 * auto-detection for exactly this case; unset in local dev, where the host
 * IS the target.
 */
function resolveTarget() {
  const envTriple = process.env.PREP_TARGET_TRIPLE

  let rustArch, rustOs, distArch, isWindows
  if (envTriple) {
    const [triArch, ...osParts] = envTriple.split('-')
    rustArch = triArch
    rustOs = osParts.join('-')
    isWindows = rustOs.includes('windows')
    distArch = process.env.PREP_NODE_ARCH || RUST_ARCH_TO_DIST[triArch]
    if (!distArch) {
      throw new Error(`cannot infer a Node dist arch from PREP_TARGET_TRIPLE="${envTriple}" — pass PREP_NODE_ARCH explicitly`)
    }
    log(`using PREP_TARGET_TRIPLE override: ${envTriple} (node dist arch: ${distArch})`)
  } else {
    const platform = process.platform
    const arch = process.arch
    rustOs = PLATFORM_RUST_OS[platform]
    rustArch = ARCH_RUST[arch]
    distArch = ARCH_DIST[arch]
    isWindows = platform === 'win32'
    if (!rustOs || !rustArch || !distArch) {
      throw new Error(`unsupported platform/arch combination: ${platform}/${arch}`)
    }
  }

  const distOs = isWindows ? 'win' : rustOs.includes('darwin') ? 'darwin' : 'linux'
  const distName = `node-v${NODE_VERSION}-${distOs}-${distArch}`
  return {
    isWindows,
    archiveExt: isWindows ? 'zip' : 'tar.gz',
    distName,
    downloadUrl: `https://nodejs.org/dist/v${NODE_VERSION}/${distName}.${isWindows ? 'zip' : 'tar.gz'}`,
    extractedBinRelative: isWindows ? path.join(distName, 'node.exe') : path.join(distName, 'bin', 'node'),
    rustTriple: `${rustArch}-${rustOs}`,
    destBinaryName: `node-${rustArch}-${rustOs}${isWindows ? '.exe' : ''}`,
  }
}

// --- (a) Node sidecar binary --------------------------------------------------

async function downloadFile(url, destPath) {
  log(`downloading ${url}`)
  const res = await fetch(url)
  if (!res.ok) throw new Error(`download failed: ${res.status} ${res.statusText} (${url})`)
  const buf = Buffer.from(await res.arrayBuffer())
  writeFileSync(destPath, buf)
}

function extractArchive(archivePath, destDir) {
  mkdirSync(destDir, { recursive: true })
  // `tar` on modern Windows (bsdtar, built in since Win10 1803) reads .zip
  // as well as .tar.gz via the same `-xf` invocation, so one code path
  // covers every platform this script targets — no extra npm dependency,
  // no platform-specific PowerShell/unzip branching. On Windows we pin the
  // absolute System32 path rather than bare `tar`: if this script runs from
  // a shell whose PATH puts a POSIX/GNU tar ahead of the OS one (Git Bash,
  // MSYS, WSL-adjacent tooling — hit during development of this script),
  // GNU tar's remote-archive heuristic misparses a `C:\...` destination as
  // `host:path` ("Cannot connect to C: resolve failed") and fails outright.
  // bsdtar has no such heuristic, so addressing it explicitly sidesteps the
  // whole class of PATH-ordering surprise instead of guessing flags.
  const tarBin =
    process.platform === 'win32' ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe') : 'tar'
  execFileSync(tarBin, ['-xf', archivePath, '-C', destDir], { stdio: 'inherit' })
}

async function prepareNodeBinary() {
  const target = resolveTarget()
  const binariesDir = path.join(SRC_TAURI, 'binaries')
  mkdirSync(binariesDir, { recursive: true })
  const destPath = path.join(binariesDir, target.destBinaryName)

  if (existsSync(destPath) && !FORCE) {
    log(`node sidecar already present at ${destPath} (use --force to redownload)`)
    return
  }

  const scratch = mkdtempSync(path.join(os.tmpdir(), 'agiletasker-node-dl-'))
  try {
    const archivePath = path.join(scratch, `node.${target.archiveExt}`)
    await downloadFile(target.downloadUrl, archivePath)
    extractArchive(archivePath, scratch)
    const extractedBin = path.join(scratch, target.extractedBinRelative)
    if (!existsSync(extractedBin)) {
      throw new Error(`expected extracted binary not found at ${extractedBin} — archive layout may have changed`)
    }
    cpSync(extractedBin, destPath)
    if (!target.isWindows) execFileSync('chmod', ['+x', destPath])

    // Prove it actually runs before declaring success — a corrupt download
    // or wrong-arch binary should fail loudly here, not at `tauri build`
    // bundling time or (worse) inside a shipped app. Skipped for a
    // cross-arch target (PREP_TARGET_TRIPLE naming a different arch than
    // this host's own): e.g. desktop/ci/release.yml builds the
    // x86_64-apple-darwin leg on an arm64 runner, which can only execute
    // that binary via Rosetta 2 — present on GitHub's macOS runners today,
    // but not something this script should hard-depend on. A byte-size
    // sanity check still catches a truncated/corrupt download either way.
    const hostArch = RUST_ARCH_TO_DIST[process.arch] ? process.arch : null
    const targetArch = target.rustTriple.split('-')[0]
    const canExecuteNatively = !process.env.PREP_TARGET_TRIPLE || targetArch === hostArch
    if (canExecuteNatively) {
      const versionOutput = execFileSync(destPath, ['--version'], { encoding: 'utf8' }).trim()
      if (!versionOutput.startsWith('v')) {
        throw new Error(`unexpected --version output from extracted node binary: "${versionOutput}"`)
      }
      log(`node sidecar ready: ${destPath} (${versionOutput})`)
    } else {
      const { size } = statSync(destPath)
      if (size < 1_000_000) {
        throw new Error(`extracted node binary at ${destPath} is suspiciously small (${size} bytes) — likely a corrupt download`)
      }
      log(`node sidecar ready: ${destPath} (cross-arch target ${target.rustTriple} on ${process.arch} host — skipped exec verification, size OK: ${size} bytes)`)
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
}

// --- (b) daemon native deps (node-pty, werift) -------------------------------

function prepareDaemonDeps() {
  const destNodeModules = path.join(SRC_TAURI, 'resources', 'daemon-deps', 'node_modules')

  if (existsSync(destNodeModules) && !FORCE) {
    log(`daemon-deps node_modules already present at ${destNodeModules} (use --force to redo)`)
    return
  }

  const scratch = mkdtempSync(path.join(os.tmpdir(), 'agiletasker-daemon-deps-'))
  try {
    writeFileSync(
      path.join(scratch, 'package.json'),
      JSON.stringify({ name: 'agiletasker-desktop-daemon-deps', private: true, version: '0.0.0' }, null, 2),
    )
    log(`npm install ${DAEMON_DEPS.join(' ')} (staging: ${scratch})`)
    // npm ships as a .cmd shim on Windows, which needs a shell to resolve
    // (same reason agent-daemon.mjs shells out to `claude` with
    // shell:true) — tried naming `npm.cmd` directly to dodge Node's
    // DEP0190 "unescaped shell args" warning, but that hit a bare
    // `spawnSync npm.cmd EINVAL` when this script is invoked directly from
    // a Git Bash/MSYS shell (as opposed to via `npm run prepare`, which
    // already runs under cmd.exe and masked the problem). shell:true is
    // the version actually verified working under both invocation paths;
    // the warning is harmless here since every arg is a static literal,
    // never interpolated user input.
    execFileSync('npm', ['install', '--omit=dev', ...DAEMON_DEPS], {
      cwd: scratch,
      stdio: 'inherit',
      shell: process.platform === 'win32',
    })

    const destParent = path.dirname(destNodeModules)
    rmSync(destNodeModules, { recursive: true, force: true })
    mkdirSync(destParent, { recursive: true })
    cpSync(path.join(scratch, 'node_modules'), destNodeModules, { recursive: true })
    log(`daemon-deps ready: ${destNodeModules}`)
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
}

// --- main ---------------------------------------------------------------------

try {
  await prepareNodeBinary()
  prepareDaemonDeps()
  log('done')
} catch (e) {
  console.error(`[prepare-deps] FAILED: ${e?.stack || e?.message || e}`)
  process.exit(1)
}
