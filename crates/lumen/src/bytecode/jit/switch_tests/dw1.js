(function(){
var out=[];
var n=3,c=0;do{c++}while(--n);out.push(c+','+n);
n=5;c=0;do{c++;if(c%2)continue;c+=10}while(--n);out.push(c+','+n);
var t=0;do{t++}while(t++<4);out.push(t);
var seen=[];n=0;do{seen.push(n)}while(seen.push('t'+n),++n<3);out.push(seen.join());
n=3;c=0;outer:do{c++;do{if(c>1)continue outer;}while(false)}while(--n);out.push(c+','+n);
return out.join(' ');
})()
