use lumen_runtime::{Completion, Runtime};

#[test]
fn native_socket_callbacks_retain_resource_scopes_across_successive_reads() {
    let mut runtime = Runtime::new();
    match runtime.eval(r#"
const assert=require('node:assert/strict');const net=require('node:net');
const {AsyncLocalStorage}=require('node:async_hooks');const storage=new AsyncLocalStorage();
var result='pending';const errors=[];let client;const sockets=[];let deadline;
const check=expected=>{if(storage.getStore()!==expected)errors.push(`${expected}:${storage.getStore()}`)};
const server=storage.run('server',()=>net.createServer(socket=>{
  sockets.push(socket);check('server');
  socket.on('data',chunk=>{check('server');socket.write(chunk)});
}).listen(0,'127.0.0.1',()=>{
  check('server');client=storage.run('client',()=>net.connect(server.address().port,'127.0.0.1'));
  const chunks=[];
  client.on('connect',()=>{check('client');client.write('first')});
  client.on('data',chunk=>{
    check('client');chunks.push(chunk.toString());
    if(chunks.length===1)storage.run('intruder',()=>client.write('second'));
    else {clearTimeout(deadline);client.destroy();for(const socket of sockets)socket.destroy();server.close();result=errors.length?errors.join(','):chunks.join(',')}
  });
}));
deadline=setTimeout(()=>{client?.destroy();for(const socket of sockets)socket.destroy();server.close();result='deadline'},5000);
"#).expect("parse socket scope fixture") {
        Completion::Value(_) => {},
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    assert!(
        matches!(runtime.eval("result").unwrap(), Completion::Value(value) if value == "first,second")
    );
}
