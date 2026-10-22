(function(){
var out=[];
var ctors=[Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,Int32Array,Uint32Array,Float32Array,Float64Array];
function fmt(v){return typeof v==='number'&&v===0&&1/v<0?'-0':String(v)}
function rd(a,k){return a[k]}
function wr(a,k,v){a[k]=v}
var keys=[0,1,3,4,-1,1.5,NaN,Infinity,-Infinity,-0,2**32,2**32-1,2**53,1e21,0.1];
var vals=[300.7,-5,1.5,2.5,NaN,-129,70000,2**33+3,1.1,Infinity,-0,'7',true,null,undefined,{valueOf:function(){return 42}}];
for(var c=0;c<ctors.length;c++){
  var C=ctors[c],a=new C(4),row=[C.name];
  for(var vi=0;vi<vals.length;vi++){
    for(var r=0;r<40;r++){wr(a,1,vals[vi]);}
    row.push(fmt(rd(a,1)));
  }
  for(var ki=0;ki<keys.length;ki++){
    for(var r=0;r<40;r++){wr(a,keys[ki],9);}
    row.push(fmt(rd(a,keys[ki])));
  }
  a['1.0']=5;a['-0']=6;a['1e1']=8;a['NaN']=3;
  row.push(fmt(a['1.0']),fmt(a['-0']),fmt(a['1e1']),fmt(a['NaN']),Object.keys(a).join(''),a.length,Array.prototype.join.call(a));
  out.push(row.join(' '));
}
var big=[BigInt64Array,BigUint64Array];
for(var c=0;c<2;c++){
  var b=new big[c](3),row=[big[c].name];
  for(var r=0;r<40;r++){wr(b,0,5n);wr(b,1,2n**64n+1n);wr(b,-1,7n);wr(b,3,7n);wr(b,1.5,7n)}
  row.push(String(rd(b,0)),String(rd(b,1)),String(rd(b,2)),String(rd(b,-1)),String(rd(b,3)),String(rd(b,1.5)),String(rd(b,NaN)));
  var e1='',e2='';
  try{b[0]=1}catch(e){e1=e.name}
  try{b[10]=1}catch(e){e2=e.name}
  try{b[-1]='x'}catch(e){e2+=e.name}
  b[0]=-1n;row.push(String(b[0]),e1,e2);
  out.push(row.join(' '));
}
var calls=0,o={valueOf:function(){calls++;return 3}};
var u=new Uint8Array(2);
for(var r=0;r<10;r++){u[5]=o;u[-1]=o;u[0.5]=o;u[0]=o}
out.push('coerce '+calls+' '+u.join());
var buf=new ArrayBuffer(4,{maxByteLength:16}),t=new Uint8Array(buf),f=new Uint8Array(buf,0,4),lt=new Uint8Array(buf,2);
var row=['rs'];
function snap(){return [t.length,f.length,lt.length,fmt(t[5]),fmt(t[3]),fmt(f[3]),fmt(lt[1]),fmt(lt[2])].join(',')}
row.push(snap());
for(var r=0;r<40;r++){t[3]=7;t[5]=8}
buf.resize(8);for(var r=0;r<40;r++){t[5]=8}
row.push(snap(),t[5],t[7]);
buf.resize(2);row.push(snap());
for(var r=0;r<40;r++){t[3]=1;f[3]=1;lt[1]=1}
buf.resize(6);row.push(snap());
var shrink={valueOf:function(){buf.resize(1);return 9}};buf.resize(6);t[4]=shrink;row.push(snap(),buf.byteLength);
out.push(row.join(' '));
var d=new ArrayBuffer(8),da=new Uint8Array(d),row2=['det'];
da[2]=5;for(var r=0;r<40;r++)rd(da,2);
var moved=d.transfer?d.transfer():null;
if(moved){for(var r=0;r<40;r++){da[2]=9;rd(da,2)}row2.push(fmt(da[2]),fmt(da[-1]),da.length,fmt(rd(da,0)));da[0]=1;row2.push(fmt(da[0]),moved.byteLength)}
out.push(row2.join(' '));
var sh=typeof SharedArrayBuffer==='function'?new Int32Array(new SharedArrayBuffer(8)):null;
if(sh){for(var r=0;r<40;r++){wr(sh,1,5);wr(sh,2,5);wr(sh,-1,5)}out.push('sab '+rd(sh,1)+' '+fmt(rd(sh,2))+' '+fmt(rd(sh,-1)))}
var arr=[1,2,3];arr[-1]='n';arr[1.5]='f';
out.push('arr '+arr[-1]+arr[1.5]+arr[5]+arr[NaN]);
var x1=new Uint8Array([1,2,300,-1,4.9]),x2=Uint8Array.from([5,6,7]),x3=new Float32Array([1.1,'2',true,null,undefined]),x4=Uint8Array.from([1,2,3],function(v,k){return v*10+k}),x5=new Uint8ClampedArray([300,-5,1.5,2.5,NaN]),x6=new BigInt64Array([1n,2n]);
var sparse=[1,,3];var x7=new Uint8Array(sparse);
Array.prototype[Symbol.iterator]=function*(){yield 42};
var x8=new Uint8Array([1,2,3]);
delete Array.prototype[Symbol.iterator];
out.push([x1,x2,x3,x4,x5,x6,x7,x8].map(function(x){return Array.prototype.join.call(x)}).join('/'));
return out.join('\n');
})()
