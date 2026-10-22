(function(){
var out=[];
function sw(x){
  switch(x){
    case 0:return 'zero';
    case 1:return 'one';
    case 2:return 'two';
    case 'a':return 'A';
    case 'b':return 'B';
    case 3.5:return 'f';
    case -1:return 'neg';
    case 100000:return 'big';
    case true:return 'T';
    case null:return 'N';
    case undefined:return 'U';
    default:return 'dflt';
  }
}
var inputs=[0,-0,1,2,'a','b','a'+'', 'ab'.slice(0,1),3.5,-1,100000,1e5,0.5,NaN,Infinity,-Infinity,'1',1.0000000001,true,false,null,undefined,{},[],1n,'0',4096,4095,-4096,2**32,2**53];
for(var r=0;r<3;r++)for(var k=0;k<inputs.length;k++){var s=sw(inputs[k]);if(r==2)out.push(s)}
function nan(x){switch(x){case 1:return 1;case 2:return 2;case 3:return 3;case NaN:return 'nan';case 4:return 4;default:return 'd'}}
function zero(x){switch(x){case 1:return 1;case 2:return 2;case 3:return 3;case -0:return 'z';case 4:return 4;default:return 'd'}}
function zero2(x){switch(x){case 1:return 1;case 2:return 2;case 3:return 3;case 0:return 'z';case 4:return 4;default:return 'd'}}
for(var r=0;r<3;r++){var o=[nan(NaN),nan(0/0),nan(1),zero(0),zero(-0),zero2(-0),zero2(0),zero(0.0*-1)];if(r==2)out.push(o.join())}
function dup(x){switch(x){case 5:return 'a';case 5:return 'b';case 6:return 'c';case 7:return 'd';case 8:return 'e'}return 'none'}
for(var r=0;r<3;r++){var o=[dup(5),dup(6),dup(9)];if(r==2)out.push(o.join())}
function str(x){switch(x){case 'alpha':return 1;case 'beta':return 2;case 'gamma':return 3;case 'delta':return 4;case '':return 5;case 'é':return 6;}return 0}
for(var r=0;r<3;r++){var o=[str('al'+'pha'),str('b'+'eta'),str(''),str('é'),str('é'),str(new String('beta')),str('Alpha')];if(r==2)out.push(o.join())}
return out.join('|');
})()
