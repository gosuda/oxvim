//! `vim.wait()` must observe terminal-job output while the wait is in
//! progress: the wait pump (`Scheduler::pump_scheduled`) flushes PTY output,
//! mirroring upstream `LOOP_PROCESS_EVENTS`.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, thread, time::Duration};

use rmpv::Value;

struct Embedded {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: i64,
    _xdg: ScratchDirectory,
}

struct ScratchDirectory(std::path::PathBuf);

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl Embedded {
    #[expect(
        clippy::expect_used,
        reason = "harness setup cannot continue without a spawned embedded process"
    )]
    fn spawn() -> Self {
        let xdg_root = std::env::temp_dir().join(format!(
            "oxvim-wait-pty-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        for name in [
            "XDG_STATE_HOME",
            "XDG_RUNTIME_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
        ] {
            let dir = xdg_root.join(name.split('_').next().unwrap_or(name).to_ascii_lowercase());
            let _ = std::fs::create_dir_all(&dir);
        }
        let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxvim"));
        command
            .arg("--embed")
            .env("OXVIM_RUNTIME", runtime)
            .env("XDG_STATE_HOME", xdg_root.join("state"))
            .env("XDG_RUNTIME_HOME", xdg_root.join("runtime"))
            .env("XDG_DATA_HOME", xdg_root.join("data"))
            .env("XDG_CONFIG_HOME", xdg_root.join("config"));
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxvim --embed");
        let input = child.stdin.take().expect("embedded stdin");
        let output = BufReader::new(child.stdout.take().expect("embedded stdout"));
        Self {
            child,
            input,
            output,
            next_id: 1,
            _xdg: ScratchDirectory(xdg_root),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "RPC transport shape is an assertion in this harness"
    )]
    fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.input, &request).expect("encode request");
        self.input.flush().expect("flush request");
        loop {
            let response =
                rmpv::decode::read_value(&mut self.output).expect("decode response");
            let Value::Array(fields) = response else {
                panic!("response is not an array")
            };
            if fields.first() == Some(&Value::from(2)) {
                continue;
            }
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0], Value::from(1));
            assert_eq!(fields[1], Value::from(id));
            assert_eq!(fields[2], Value::Nil, "RPC error: {:?}", fields[2]);
            return fields[3].clone();
        }
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `vim.wait()` loop over `nvim_buf_get_lines` must observe a terminal
/// job child's output while the wait is in progress, not only on a later
/// transport tick. Markers stream for the whole wait so the first wait-turn
/// pump already has bytes to flush, regardless of timer-wake spacing.
#[test]
fn vim_wait_observes_terminal_job_output() {
    let mut vim = Embedded::spawn();
    // Wait for the server to come up (nvim_exec_lua answers once Lua runs).
    for _ in 0..100 {
        let probe = vim.request("nvim_exec_lua", vec![Value::from("return 1"), Value::Array(vec![])]);
        if probe == Value::from(1) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let chunk = r#"
        vim.cmd('enew')
        local buf = vim.api.nvim_get_current_buf()
        local job = vim.fn.jobstart(
            { 'sh', '-c', 'echo GO; for i in 1 2 3 4 5; do sleep 0.2; echo MARKER$i; done' },
            { term = true }
        )
        assert(job > 0, 'jobstart failed')
        local iters = 0
        local ok = vim.wait(15000, function()
            iters = iters + 1
            for _, l in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
                if l:find('MARKER', 1, true) or l:find('GO', 1, true) then
                    return true
                end
            end
            return false
        end, 20)
        local seen = 'none'
        for _, l in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
            local m = l:match('(MARKER%d+)') or (l:find('GO', 1, true) and 'GO' or nil)
            if m then seen = m end
        end
        return 'ok=' .. tostring(ok) .. ' iters=' .. iters .. ' seen=' .. seen
    "#;
    let result = vim.request(
        "nvim_exec_lua",
        vec![Value::from(chunk), Value::Array(vec![])],
    );
    let Value::String(text) = result else {
        panic!("expected a string summary, got {result:?}")
    };
    let text = text.as_str().expect("summary is utf-8");
    assert!(
        text.starts_with("ok=true"),
        "vim.wait never observed terminal output: {text}"
    );
    assert!(
        !text.contains("seen=none"),
        "terminal buffer has no marker after a successful wait: {text}"
    );
}

/// Both Ex slots are borrowed while `vim.wait` runs, yet the wait still
/// observes terminal output: the outer `vim.cmd` holds one executor across
/// its inner `:lua`, the inner `vim.cmd` holds the other across the wait, so
/// no `try_borrow_mut` flush can run and only the borrow-free scheduler pump
/// delivers bytes. The marker release is kernel-synchronised, not timed: the
/// child blocks opening the FIFO for reading until the predicate opens it
/// for writing, so the first predicate turn deterministically releases the
/// marker and a later turn must observe it before the timeout.
#[test]
fn vim_wait_observes_terminal_job_output_with_both_executors_busy() {
    let mut vim = Embedded::spawn();
    // Wait for the server to come up (nvim_exec_lua answers once Lua runs).
    for _ in 0..100 {
        let probe = vim.request("nvim_exec_lua", vec![Value::from("return 1"), Value::Array(vec![])]);
        if probe == Value::from(1) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let chunk = r#"
        vim.cmd('enew')
        local buf = vim.api.nvim_get_current_buf()
        local fifo = vim.fn.tempname() .. '.fifo'
        vim.fn.system({ 'mkfifo', fifo })
        local job = vim.fn.jobstart(
            { 'sh', '-c', 'read line < "' .. fifo .. '"; echo "MARKER-$line"' },
            { term = true }
        )
        assert(job > 0, 'jobstart failed')
        _G._bb_buf = buf
        _G._bb_fifo = fifo
        _G._bb_released = false
        _G._bb_pred = function()
            if not _G._bb_released then
                local f = io.open(_G._bb_fifo, 'w')
                if f then
                    f:write('GO\n')
                    f:close()
                end
                _G._bb_released = true
            end
            for _, l in ipairs(vim.api.nvim_buf_get_lines(_G._bb_buf, 0, -1, false)) do
                if l:find('MARKER', 1, true) then
                    return true
                end
            end
            return false
        end
        -- Two nested `vim.cmd` levels hold both Ex executors across the wait:
        -- the outer `:lua` keeps the first slot busy while the inner `:lua`
        -- keeps the second, so executor-bound flushes cannot run.
        vim.cmd('lua vim.cmd("lua _G._bb_ok = vim.wait(15000, _G._bb_pred, 20)")')
        local seen = 'none'
        for _, l in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
            local m = l:match('(MARKER[^%s]*)')
            if m then
                seen = m
            end
        end
        vim.fn.delete(fifo)
        return 'ok=' .. tostring(_G._bb_ok) .. ' seen=' .. seen
    "#;
    let result = vim.request(
        "nvim_exec_lua",
        vec![Value::from(chunk), Value::Array(vec![])],
    );
    let Value::String(text) = result else {
        panic!("expected a string summary, got {result:?}")
    };
    let text = text.as_str().expect("summary is utf-8");
    assert!(
        text.starts_with("ok=true"),
        "vim.wait with both executors busy never observed terminal output: {text}"
    );
    assert!(
        !text.contains("seen=none"),
        "terminal buffer has no marker after a successful both-busy wait: {text}"
    );
}
