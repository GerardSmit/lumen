(function(){
var out=[];
function fmt(v){return typeof v==='number'&&v===0&&1/v<0?'-0':String(v)}
function sumKeys(a,keys,n){
  var s=0,u=0,i;
  for(i=0;i<n;i++){var v=a[keys[i%keys.length]];if(v===undefined)u++;else s+=v}
  return s+':'+u;
}
function sumLocal(a,n){
  var s=0,u=0,i;
  for(i=0;i<n;i++){var v=a[i];if(v===undefined)u++;else s+=v}
  return s+':'+u;
}
function bump(){}
function sumCall(a,n,f){
  var s=0,u=0,i;
  for(i=0;i<n;i++){f(i);var v=a[i];if(v===undefined)u++;else s+=v}
  return s+':'+u+':'+a.length;
}
var a=[1,2,3,4,5,6,7,8];
for(var r=0;r<60;r++)sumLocal(a,8);
out.push(sumLocal(a,8),sumLocal(a,12));
var keys=[0,1,7,8,-1,1.5,NaN,Infinity,-0,'2','x',2**32,3];
for(var r=0;r<60;r++)sumKeys(a,keys,keys.length);
out.push(sumKeys(a,keys,keys.length*3));
var h=[1,2,,4,5,,7,8];
for(var r=0;r<60;r++)sumLocal(h,8);
out.push(sumLocal(h,8));
var nn=[1,NaN,3,undefined,5,'6',null,8];
for(var r=0;r<60;r++)sumLocal(nn,8);
out.push(sumLocal(nn,8));
var fl=[1.5,2.5,-0,4.25,5,6,7,8];
for(var r=0;r<60;r++)sumLocal(fl,8);
out.push(fmt(+sumLocal(fl,8).split(':')[0]));
for(var r=0;r<60;r++)sumLocal(a,8);
Array.prototype[3]=100;
var p=[1,2,3,4,5,6,7,8];delete p[3];
out.push(sumLocal(p,8),sumLocal([1,2,3,,5],8));
delete Array.prototype[3];
out.push(sumLocal(p,8));
var g=[1,2,3,4,5,6,7,8];
for(var r=0;r<60;r++)sumCall(g,8,bump);
out.push(sumCall(g,8,bump));
var g2=[1,2,3,4,5,6,7,8];
out.push(sumCall(g2,8,function(i){if(i===3)g2.length=2}));
var g3=[1,2,3,4,5,6,7,8];
out.push(sumCall(g3,8,function(i){if(i===2)delete g3[5]}));
var g4=[1,2,3,4,5,6,7,8];
out.push(sumCall(g4,8,function(i){if(i===2)g4[6]='x'}));
var g5=[1,2,3,4,5,6,7,8];
out.push(sumCall(g5,8,function(i){if(i===2)g5.push(9,10)}));
var g6=[1,2,3,4,5,6,7,8];
out.push(sumCall(g6,8,function(i){if(i===4)Object.defineProperty(g6,6,{get:function(){return 1000}})}));
var g7=[1,2,3,4,5,6,7,8];
out.push(sumCall(g7,8,function(i){if(i===1)Array.prototype[5]=50;if(i===1)delete g7[5]}));
delete Array.prototype[5];
var big=[];for(var i=0;i<20;i++)big.push(i);
var small=[1,2,3];
function swap(x,n){
  var s=0,i;
  for(i=0;i<n;i++){var v=x[i];s+=v===undefined?-1:v}
  return s;
}
for(var r=0;r<60;r++)swap(big,20);
out.push(swap(big,20),swap(small,20),swap(big,20),swap(h,20));
function twice(){
  var q=[1,2,3],s=0;
  for(var i=0;i<6;i++){s+=q[i]===undefined?0:q[i];if(i===1)q=[10,20,30,40,50,60]}
  return s;
}
for(var r=0;r<60;r++)twice();
out.push(twice());
var ga=[1,2,3,4];
function gl(n){var s=0;for(var i=0;i<n;i++){var v=ga[i];s+=v===undefined?-1:v;if(i===2&&n===6)ga=[9,9,9,9,9,9]}return s}
for(var r=0;r<60;r++)gl(4);
out.push(gl(4),gl(6),gl(6));
function unset(n){
  var s;
  var t;
  for(var i=0;i<n;i++){if(i===0){s=1;t=2}else{s+=t;t+=s}}
  return s+':'+t;
}
for(var r=0;r<60;r++)unset(10);
out.push(unset(10),unset(1),unset(0));
function ush(x,n){
  var s=0;
  for(var i=0;i<n;i++){s+=(x+i)>>>8;s+=(x-i)>>>0;s+=(x*i)>>>31;s+=(x+i)>>>32;s+=(x-i)>>>33}
  return s;
}
for(var r=0;r<60;r++)ush(123456,10);
out.push(ush(123456,10),ush(-5,10),ush(2**31,10),ush(-(2**40),10),ush(NaN,3),ush(2**53,5));
return out.join('\n');
})()
