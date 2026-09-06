(function(){
'use strict';

var STORAGE_KEY='rentalScenarioCalculator.v3';
var OLD_STORAGE_KEY='rentalScenarioCalculator.v2';
var defaults={
  name:'Scenario 1',purchasePrice:450000,downPaymentPct:20,closingCostsPct:3,renovation:25000,
  mortgageRate:6.5,mortgageTerm:30,pmiPct:0.6,
  rentMonthly:3200,vacancyPct:5,rentGrowthPct:3,
  propertyTaxAnnual:6750,insuranceAnnual:1800,hoaMonthly:0,maintenancePct:5,capexPct:5,
  utilitiesMonthly:0,otherExpensesMonthly:0,expenseGrowthPct:2.5
};
var state={scenarios:[clone(defaults)],active:0,compare:false};
var allowed={};Object.keys(defaults).forEach(function(k){allowed[k]=true;});

function clone(o){return JSON.parse(JSON.stringify(o));}
function byId(id){return document.getElementById(id);}
function esc(s){return String(s).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];});}
function n(v){v=parseFloat(v);return isFinite(v)?v:0;}
function money(v,d){if(d===undefined)d=0;if(!isFinite(v))return '—';return new Intl.NumberFormat('en-US',{style:'currency',currency:'USD',minimumFractionDigits:d,maximumFractionDigits:d}).format(v);}
function pct(v,d){if(d===undefined)d=1;return isFinite(v)?v.toFixed(d)+'%':'—';}
function sanitizeScenario(src,index){var out=clone(defaults),k;if(src&&typeof src==='object'){for(k in src){if(allowed[k])out[k]=src[k];}}if(!out.name)out.name='Scenario '+(index+1);if([15,20,30].indexOf(n(out.mortgageTerm))<0)out.mortgageTerm=30;if([0,0.6,1,1.5].indexOf(n(out.pmiPct))<0)out.pmiPct=0.6;return out;}
function load(){
  var parsed=null;try{parsed=JSON.parse(localStorage.getItem(STORAGE_KEY)||'null');}catch(e){}
  if(!parsed){try{parsed=JSON.parse(localStorage.getItem(OLD_STORAGE_KEY)||'null');}catch(e){}}
  if(parsed&&parsed.scenarios&&parsed.scenarios.length){state.scenarios=parsed.scenarios.map(sanitizeScenario);state.active=Math.min(Math.max(0,n(parsed.active)),state.scenarios.length-1);state.compare=!!parsed.compare;}
}
function save(){try{localStorage.setItem(STORAGE_KEY,JSON.stringify(state));}catch(e){}}
function pmt(principal,annualRate,years){if(principal<=0||years<=0)return 0;var months=years*12,r=annualRate/100/12;if(r===0)return principal/months;var x=Math.pow(1+r,months);return principal*r*x/(x-1);}
function loanBalance(principal,annualRate,years,paid){if(principal<=0)return 0;var months=years*12;paid=Math.min(Math.max(0,paid),months);if(paid>=months)return 0;var r=annualRate/100/12;if(r===0)return principal*(1-paid/months);var pay=pmt(principal,annualRate,years),x=Math.pow(1+r,paid);return Math.max(0,principal*x-pay*(x-1)/r);}
function pmiForMonth(s,loan,monthIndex){if(n(s.pmiPct)<=0||n(s.downPaymentPct)>=20)return 0;var startBal=loanBalance(loan,n(s.mortgageRate),n(s.mortgageTerm),monthIndex);if(startBal<=n(s.purchasePrice)*0.80)return 0;return loan*n(s.pmiPct)/100/12;}
function yearProjection(s,loan,mortgage,year){
  var rf=Math.pow(1+n(s.rentGrowthPct)/100,year-1),ef=Math.pow(1+n(s.expenseGrowthPct)/100,year-1);
  var rent=n(s.rentMonthly)*rf,vacancy=rent*n(s.vacancyPct)/100,effective=rent-vacancy;
  var taxes=n(s.propertyTaxAnnual)/12*ef,insurance=n(s.insuranceAnnual)/12*ef,hoa=n(s.hoaMonthly)*ef,utilities=n(s.utilitiesMonthly)*ef,other=n(s.otherExpensesMonthly)*ef;
  var maintenance=rent*n(s.maintenancePct)/100,capex=rent*n(s.capexPct)/100;
  var opEx=taxes+insurance+hoa+utilities+other+maintenance+capex,noi=effective-opEx;
  var pmiTotal=0,m;for(m=(year-1)*12;m<year*12;m++)pmiTotal+=pmiForMonth(s,loan,m);
  var monthsLeft=Math.max(0,n(s.mortgageTerm)*12-(year-1)*12),mortgageMonths=Math.min(12,monthsLeft),debt=mortgage*mortgageMonths;
  var cashFlow=noi*12-debt-pmiTotal;
  var endBal=loanBalance(loan,n(s.mortgageRate),n(s.mortgageTerm),year*12);
  return {year:year,rent:rent,noiMonthly:noi,cashFlowAnnual:cashFlow,cashFlowMonthly:cashFlow/12,pmiMonthly:pmiTotal/12,balance:endBal};
}
function calc(s){
  var price=n(s.purchasePrice),down=price*n(s.downPaymentPct)/100,loan=Math.max(0,price-down),closing=price*n(s.closingCostsPct)/100,cashNeeded=down+closing+n(s.renovation),mortgage=pmt(loan,n(s.mortgageRate),n(s.mortgageTerm));
  var rent=n(s.rentMonthly),vacancy=rent*n(s.vacancyPct)/100,effective=rent-vacancy;
  var maintenance=rent*n(s.maintenancePct)/100,capex=rent*n(s.capexPct)/100;
  var fixed=n(s.propertyTaxAnnual)/12+n(s.insuranceAnnual)/12+n(s.hoaMonthly)+n(s.utilitiesMonthly)+n(s.otherExpensesMonthly);
  var opEx=fixed+maintenance+capex,noi=effective-opEx,pmiMo=pmiForMonth(s,loan,0),cashFlow=noi-mortgage-pmiMo;
  var capRate=price>0?noi*12/price*100:NaN,coc=cashNeeded>0?cashFlow*12/cashNeeded*100:NaN;
  var marginal=1-n(s.vacancyPct)/100-n(s.maintenancePct)/100-n(s.capexPct)/100;
  var breakEven=marginal>0?(fixed+mortgage+pmiMo)/marginal:NaN;
  var years=[];for(var y=1;y<=5;y++)years.push(yearProjection(s,loan,mortgage,y));
  return {down:down,loan:loan,closing:closing,cashNeeded:cashNeeded,mortgage:mortgage,pmiMo:pmiMo,rent:rent,vacancy:vacancy,effective:effective,maintenance:maintenance,capex:capex,fixed:fixed,opEx:opEx,noi:noi,cashFlow:cashFlow,capRate:capRate,coc:coc,breakEven:breakEven,years:years};
}
function row(label,value,total){return '<div class="row'+(total?' total':'')+'"><span>'+esc(label)+'</span><span>'+value+'</span></div>';}
function setTone(el,value){el.classList.remove('good','bad');el.classList.add(value>=0?'good':'bad');}

function readInputs(){var els=document.querySelectorAll('[data-key]'),s=state.scenarios[state.active],i,key;for(i=0;i<els.length;i++){key=els[i].getAttribute('data-key');s[key]=els[i].type==='text'?els[i].value:n(els[i].value);}save();}
function writeInputs(){var els=document.querySelectorAll('[data-key]'),s=state.scenarios[state.active],i,key;for(i=0;i<els.length;i++){key=els[i].getAttribute('data-key');if(s[key]!==undefined)els[i].value=s[key];}syncChoices();}
function syncChoices(){var s=state.scenarios[state.active],buttons=document.querySelectorAll('[data-choice]');for(var i=0;i<buttons.length;i++){var key=buttons[i].getAttribute('data-choice'),value=n(buttons[i].getAttribute('data-value'));buttons[i].classList.toggle('active',n(s[key])===value);buttons[i].setAttribute('aria-pressed',n(s[key])===value?'true':'false');}}
function bindChoices(){var buttons=document.querySelectorAll('[data-choice]');for(var i=0;i<buttons.length;i++)buttons[i].onclick=function(){var s=state.scenarios[state.active],key=this.getAttribute('data-choice');s[key]=n(this.getAttribute('data-value'));save();syncChoices();render();};}
function renderTabs(){var html='',i;for(i=0;i<state.scenarios.length;i++)html+='<button class="tab'+(i===state.active?' active':'')+'" data-tab="'+i+'">'+esc(state.scenarios[i].name||('Scenario '+(i+1)))+'</button>';byId('tabs').innerHTML=html;var tabs=document.querySelectorAll('[data-tab]');for(i=0;i<tabs.length;i++)tabs[i].onclick=function(){state.active=parseInt(this.getAttribute('data-tab'),10);save();renderAll();};}
function renderOutlook(c){var html='';for(var i=0;i<c.years.length;i++){var r=c.years[i];html+='<div class="outlook-year"><span class="year">Year '+r.year+'</span><strong class="'+(r.cashFlowMonthly>=0?'good':'bad')+'">'+money(r.cashFlowMonthly)+'</strong><small>'+money(r.rent)+' rent/mo</small></div>';}byId('outlook').innerHTML=html;}
function render(){
  var s=state.scenarios[state.active],c=calc(s);
  byId('resultName').textContent=s.name||'Scenario';
  byId('mCashFlow').textContent=money(c.cashFlow);setTone(byId('mCashFlow'),c.cashFlow);
  byId('mCoc').textContent=pct(c.coc);setTone(byId('mCoc'),c.coc);
  byId('mCap').textContent=pct(c.capRate);
  byId('mCashNeeded').textContent=money(c.cashNeeded);
  byId('mMortgage').textContent=money(c.mortgage);
  byId('mBreakEven').textContent=money(c.breakEven);
  byId('mobileCashFlow').textContent=money(c.cashFlow);setTone(byId('mobileCashFlow'),c.cashFlow);
  byId('mobileCashNeeded').textContent=money(c.cashNeeded);
  byId('mobileBreakEven').textContent=money(c.breakEven);
  byId('opsRows').innerHTML=row('Scheduled rent',money(c.rent),false)+row('Vacancy','− '+money(c.vacancy),false)+row('Effective rent',money(c.effective),true)+row('Fixed owner costs','− '+money(c.fixed),false)+row('Maintenance reserve','− '+money(c.maintenance),false)+row('CapEx reserve','− '+money(c.capex),false)+row('NOI',money(c.noi),true)+row('Mortgage P&I','− '+money(c.mortgage),false)+row('PMI','− '+money(c.pmiMo),false)+row('Cash flow',money(c.cashFlow),true);
  byId('financeRows').innerHTML=row('Down payment',money(c.down),false)+row('Loan amount',money(c.loan),false)+row('Closing costs',money(c.closing),false)+row('Renovation',money(n(s.renovation)),false)+row('Cash required',money(c.cashNeeded),true)+row('Mortgage term',Math.round(n(s.mortgageTerm))+' years',false)+row('PMI estimate',n(s.pmiPct)===0?'None':pct(n(s.pmiPct),1)+' / year',false)+row('Balance after 5 years',money(c.years[4].balance),true);
  renderOutlook(c);renderCompare();
}
function renderCompare(){var panel=byId('comparePanel');panel.className='card compare'+(state.compare?' show':'');if(!state.compare)return;var labels=['Purchase price','Cash required','Rent / month','Mortgage / month','Cash flow / month','Break-even rent','Cap rate','Cash-on-cash'],html='<div class="compare-wrap"><table class="compare-table"><thead><tr><th>Metric</th>',i,j,c,s;for(i=0;i<state.scenarios.length;i++)html+='<th>'+esc(state.scenarios[i].name)+'</th>';html+='</tr></thead><tbody>';for(j=0;j<labels.length;j++){html+='<tr><td>'+labels[j]+'</td>';for(i=0;i<state.scenarios.length;i++){s=state.scenarios[i];c=calc(s);var vals=[money(n(s.purchasePrice)),money(c.cashNeeded),money(n(s.rentMonthly)),money(c.mortgage),money(c.cashFlow),money(c.breakEven),pct(c.capRate),pct(c.coc)];html+='<td>'+vals[j]+'</td>';}html+='</tr>';}html+='</tbody></table></div>';panel.innerHTML='<h2>Scenario comparison</h2>'+html;}
function renderAll(){writeInputs();renderTabs();render();byId('compareBtn').textContent=state.compare?'Hide compare':'Compare';}
function download(name,text,type){var blob=new Blob([text],{type:type}),url=URL.createObjectURL(blob),a=document.createElement('a');a.href=url;a.download=name;document.body.appendChild(a);a.click();document.body.removeChild(a);setTimeout(function(){URL.revokeObjectURL(url);},1000);}
function bind(){
  var els=document.querySelectorAll('[data-key]'),i;for(i=0;i<els.length;i++){els[i].addEventListener('input',function(){readInputs();renderTabs();render();});els[i].addEventListener('change',function(){readInputs();renderTabs();render();});}
  bindChoices();
  byId('addBtn').onclick=function(){readInputs();var s=clone(defaults);s.name='Scenario '+(state.scenarios.length+1);state.scenarios.push(s);state.active=state.scenarios.length-1;save();renderAll();};
  byId('duplicateBtn').onclick=function(){readInputs();var s=clone(state.scenarios[state.active]);s.name=(s.name||'Scenario')+' copy';state.scenarios.push(s);state.active=state.scenarios.length-1;save();renderAll();};
  byId('deleteBtn').onclick=function(){if(state.scenarios.length===1){state.scenarios=[clone(defaults)];state.active=0;}else{state.scenarios.splice(state.active,1);if(state.active>=state.scenarios.length)state.active=state.scenarios.length-1;}save();renderAll();};
  byId('compareBtn').onclick=function(){readInputs();state.compare=!state.compare;save();renderAll();};
  byId('csvBtn').onclick=function(){readInputs();var c=calc(state.scenarios[state.active]),rows=[['year','rent_monthly','noi_monthly','pmi_monthly','cash_flow_monthly','mortgage_balance']],r;for(var i=0;i<c.years.length;i++){r=c.years[i];rows.push([r.year,r.rent,r.noiMonthly,r.pmiMonthly,r.cashFlowMonthly,r.balance]);}download('rental-cash-flow-outlook.csv',rows.map(function(x){return x.join(',');}).join('\n'),'text/csv');};
}
window.onerror=function(msg){var st=byId('status');if(st){st.style.display='block';st.textContent='Calculator error: '+msg;}};
load();bind();renderAll();
})();
