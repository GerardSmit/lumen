#![cfg(unix)]
use std::process::Command;

#[test]
fn dedicated_advanced_ipc_preserves_large_binary_graph_and_stdio_and_closes_cleanly() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-ipc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let child = dir.join("child.cjs");
    let parent = dir.join("parent.cjs");
    std::fs::write(
        &child,
        r#"
      console.log('separate stdout');
      process.on('message',msg=>process.send(msg,()=>process.disconnect()));
    "#,
    )
    .unwrap();
    std::fs::write(&parent,format!(r#"
      const assert=require('node:assert/strict');const {{spawn}}=require('node:child_process');
      const child=spawn(process.execPath,[{}],{{stdio:['ignore','pipe','pipe','ipc'],serialization:'advanced'}});
      let stdout='',stderr='',received=false;
      child.stdout.on('data',b=>stdout+=b);child.stderr.on('data',b=>stderr+=b);
      const timer=setTimeout(()=>{{child.kill('SIGKILL');throw new Error('IPC deadline')}},10000);
      child.on('error',e=>{{child.kill('SIGKILL');throw e}});
      const payload={{bytes:Buffer.alloc(262144,255),big:1n<<80n,set:new Set(['yes']),text:'☀️'.repeat(50000)}};
      payload.self=payload;
      child.on('message',msg=>{{assert.equal(msg.big,payload.big);assert.equal(msg.self,msg);
        assert.deepEqual(msg.bytes,payload.bytes);assert.deepEqual([...msg.set],['yes']);assert.equal(msg.text,payload.text);received=true}});
      child.on('close',code=>{{clearTimeout(timer);assert.equal(code,0);assert.equal(stderr,'');
        assert.equal(stdout.trim(),'separate stdout');assert.equal(received,true);assert.equal(child.connected,false);console.log('IPC verified')}});
      child.send(payload);
    "#,format!("{:?}",child.to_string_lossy()))).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(&parent)
        .output()
        .expect("run IPC fixture");
    let _ = std::fs::remove_dir_all(dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "IPC verified"
    );
}
