(function(){
var log=[];
function t(v){log.push(v);return v}
function eff(x){
  switch(x){
    case t(1):return 'a';
    case t(2):return 'b';
    case t(3):return 'c';
    case t(4):return 'd';
    default:return 'e';
    case t(5):return 'f';
  }
}
var res=[];
for(var r=0;r<3;r++){log=[];res=[eff(3),eff(5),eff(9),eff(1)];}
var s1=res.join()+';'+log.join();
function sm(n){
  var mode=0,acc=0,i=0,trace='';
  outer: for(i=0;i<n;i++){
    switch(mode){
      case 0: acc+=1; mode=1; break;
      case 1: acc+=2; mode=2;
      case 2: acc+=3; mode=3; break;
      default: acc+=100; mode=6; continue outer;
      case 3: acc^=i; mode=4; break;
      case 4:
        for(var j=0;j<3;j++){ switch(j){case 0:acc+=1;break;case 1:acc+=2;continue;case 2:acc+=3;break;case 7:acc=0;} acc+=10; }
        mode=5; break;
      case 5: if(i>n-5) break outer; mode=0; continue;
      case 6: acc-=7; mode=(i%2)?0:3; break;
    }
    acc|=0;
  }
  return acc+':'+mode+':'+i;
}
var r2=[];for(var q=0;q<5;q++)r2.push(sm(200+q*37));
var big=0;
for(var i=0;i<20000;i++){
  switch(i%13){case 0:big+=1;break;case 1:big+=2;break;case 2:big+=3;break;case 3:big+=5;break;case 4:big+=7;break;case 5:big-=1;break;case 6:big^=i;break;case 7:big+=11;break;case 8:big+=13;case 9:big+=17;break;case 10:big+=19;break;default:big+=23;}
}
function sws(n){var c=0,s='';for(var i=0;i<n;i++){var k=['aa','bb','cc','dd','ee'][i%5];switch(k){case 'aa':c+=1;break;case 'bb':c+=2;break;case 'cc':c+=3;break;case 'dd':c+=4;break;default:c+=5}}return c}
function dbl(n){var c=0;for(var i=0;i<n;i++){var x=i*0.5;switch(x){case 0:c+=1;break;case 0.5:c+=2;break;case 1:c+=3;break;case 1.5:c+=4;break;case 2000:c+=1000;break;default:c+=1}}return c}
function lex(n){var c=0;for(var i=0;i<n;i++){switch(i&3){case 0:let a=i;c+=a;break;case 1:c+=2;break;case 2:c+=3;break;case 3:c+=4;break;}}return c}
function thr(n){var c=0;for(var i=0;i<n;i++){try{switch(i%6){case 0:c+=1;break;case 1:c+=2;break;case 2:c+=3;break;case 3:throw 1;case 4:c+=4;break;default:c+=5}}catch(e){c+=100}}return c}
function ret(n){for(var i=0;i<n;i++){switch(i){case 0:case 1:case 2:case 3:continue;case 4:case 5:break;case 1000:return 'r'+i;default:}}return 'end'}
return [s1,r2.join(),big,sws(5000),dbl(5000),lex(100),thr(600),ret(2000)].join('|');
})()
