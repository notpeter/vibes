(function(){
'use strict';
var defaults={name:'Scenario 1',purchasePrice:450000,downPaymentPct:20,closingCostsPct:3,renovation:25000,mortgageRate:6.5,mortgageTerm:30,pmiPct:0.6,pmiMonthlyOverride:0,pmiCancelLtv:80,rentMonthly:3200,otherIncomeMonthly:0,vacancyPct:5,rentGrowthPct:3,propertyTaxAnnual:6750,insuranceAnnual:1800,hoaMonthly:0,managementPct:0,maintenancePct:5,capexPct:5,utilitiesMonthly:0,otherExpensesMonthly:0,expenseGrowthPct:2.5,analysisYears:10,appreciationPct:3,sellingCostsPct:6};
var state={scenarios:[clone(defaults)],active:0,compare:false};
function clone(o){return JSON.parse(JSON.stringify(o));}
function byId(id){return document.getElementById(id);}
function esc(s){return String(s).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];});}
function n(v){v=parseFloat(v);return isFinite(v)?v:0;}
function load(){try{var raw=localStorage.getItem('rentalScenarioCalculator.v2');if(raw){var parsed=JSON.parse(raw);if(parsed&&parsed.scenarios&&parsed.scenarios.length){state=parsed;}}}catch(e){} if(state.active<0||state.active>=state.scenarios.length)state.active=0;}
function save(){try{localStorage.setItem('rentalScenarioCalculator.v2',JSON.stringify(state));}catch(e){}}
function money(v,d){if(d===undefined)d=0;if(!isFinite(v))return '—';return new Intl.NumberFormat('en-US',{style:'currency',currency:'USD',minimumFractionDigits:d,maximumFractionDigits:d}).format(v);}
function pct(v,d){if(d===undefined)d=1;return isFinite(v)?v.toFixed(d)+'%':'—';}
function mult(v){return isFinite(v)?v.toFixed(2)+'×':'—';}
function pmt(principal,annualRate,years){if(principal<=0||years<=0)return 0;var months=years*12,r=annualRate/100/12;if(r===0)return principal/months;var x=Math.pow(1+r,months);return principal*r*x/(x-1);}
function loanBalance(principal,annualRate,years,paid){if(principal<=0)return 0;var months=years*12;paid=Math.min(Math.max(0,paid),months);if(paid>=months)return 0;var r=annualRate/100/12;if(r===0)return principal*(1-paid/months);var pay=pmt(principal,annualRate,years),x=Math.pow(1+r,paid);return Math.max(0,principal*x-pay*(x-1)/r);}
function annualIrr(cfs){function npv(rate){var sum=0,i;for(i=0;i<cfs.length;i++)sum+=cfs[i]/Math.pow(1+rate,i);return sum;}var low=-0.9999,high=10,fl=npv(low),fh=npv(high),mid,fm,i;if(!isFinite(fl)||!isFinite(fh)||fl*fh>0)return NaN;for(i=0;i<200;i++){mid=(low+high)/2;fm=npv(mid);if(Math.abs(fm)<0.000001)return mid;if(fl*fm<=0){high=mid;fh=fm;}else{low=mid;fl=fm;}}return (low+high)/2;}
function calc(s){
 var price=n(s.purchasePrice),down=price*n(s.downPaymentPct)/100,loan=Math.max(0,price-down),closing=price*n(s.closingCostsPct)/100,cashNeeded=down+closing+n(s.renovation),mortgage=pmt(loan,n(s.mortgageRate),n(s.mortgageTerm));
 var grossMo=n(s.rentMonthly)+n(s.otherIncomeMonthly),vacancyMo=grossMo*n(s.vacancyPct)/100,effectiveMo=grossMo-vacancyMo;
 var managementMo=n(s.rentMonthly)*n(s.managementPct)/100,maintenanceMo=n(s.rentMonthly)*n(s.maintenancePct)/100,capexMo=n(s.rentMonthly)*n(s.capexPct)/100;
 var opExMo=n(s.propertyTaxAnnual)/12+n(s.insuranceAnnual)/12+n(s.hoaMonthly)+managementMo+maintenanceMo+capexMo+n(s.utilitiesMonthly)+n(s.otherExpensesMonthly);
 var noiMo=effectiveMo-opExMo,initialLtv=price>0?loan/price*100:0;
 var calcPmiMo=n(s.pmiMonthlyOverride)>0?n(s.pmiMonthlyOverride):loan*n(s.pmiPct)/100/12;var pmiMo=(initialLtv>n(s.pmiCancelLtv))?calcPmiMo:0;
 var cashFlowMo=noiMo-mortgage-pmiMo,capRate=price>0?noiMo*12/price*100:NaN,coc=cashNeeded>0?cashFlowMo*12/cashNeeded*100:NaN;
 var fixedNonRent=n(s.propertyTaxAnnual)/12+n(s.insuranceAnnual)/12+n(s.hoaMonthly)+n(s.utilitiesMonthly)+n(s.otherExpensesMonthly)+mortgage+pmiMo-n(s.otherIncomeMonthly)*(1-n(s.vacancyPct)/100);
 var marginal=1-n(s.vacancyPct)/100-n(s.managementPct)/100-n(s.maintenancePct)/100-n(s.capexPct)/100;
 var breakEven=marginal>0?fixedNonRent/marginal:NaN;
 var years=Math.max(1,Math.round(n(s.analysisYears))),rows=[],cfs=[-cashNeeded],y;
 for(y=1;y<=years;y++){
   var rf=Math.pow(1+n(s.rentGrowthPct)/100,y-1),ef=Math.pow(1+n(s.expenseGrowthPct)/100,y-1),rent=n(s.rentMonthly)*rf,otherIncome=n(s.otherIncomeMonthly)*rf;
   var gross=(rent+otherIncome)*12,vacancy=gross*n(s.vacancyPct)/100,effective=gross-vacancy;
   var taxes=n(s.propertyTaxAnnual)*ef,insurance=n(s.insuranceAnnual)*ef,hoa=n(s.hoaMonthly)*12*ef,utilities=n(s.utilitiesMonthly)*12*ef,otherExp=n(s.otherExpensesMonthly)*12*ef;
   var mgmt=rent*12*n(s.managementPct)/100,maint=rent*12*n(s.maintenancePct)/100,capex=rent*12*n(s.capexPct)/100,opEx=taxes+insurance+hoa+utilities+otherExp+mgmt+maint+capex,noi=effective-opEx;
   var startBal=loanBalance(loan,n(s.mortgageRate),n(s.mortgageTerm),(y-1)*12),endBal=loanBalance(loan,n(s.mortgageRate),n(s.mortgageTerm),y*12),principal=Math.max(0,startBal-endBal);
   var monthsLeft=Math.max(0,n(s.mortgageTerm)*12-(y-1)*12),mortgageMonths=Math.min(12,monthsLeft),annualDebt=mortgage*mortgageMonths,interest=Math.max(0,annualDebt-principal);
   var value=price*Math.pow(1+n(s.appreciationPct)/100,y),yearLtv=value>0?endBal/value*100:0,annualPmi=(startBal>0&&yearLtv>n(s.pmiCancelLtv))?calcPmiMo*12:0;
   var cashFlow=noi-annualDebt-annualPmi,equity=value-endBal,sale=0,total=cashFlow;if(y===years){sale=value*(1-n(s.sellingCostsPct)/100)-endBal;total+=sale;}cfs.push(total);
   rows.push({year:y,rent:rent,gross:gross,effective:effective,opEx:opEx,noi:noi,interest:interest,principal:principal,pmi:annualPmi,cashFlow:cashFlow,value:value,balance:endBal,equity:equity,sale:sale,total:total});
 }
 var irr=annualIrr(cfs)*100,returned=0;for(y=1;y<cfs.length;y++)returned+=cfs[y];
 return {down:down,loan:loan,closing:closing,cashNeeded:cashNeeded,mortgage:mortgage,pmiMo:pmiMo,grossMo:grossMo,vacancyMo:vacancyMo,effectiveMo:effectiveMo,managementMo:managementMo,maintenanceMo:maintenanceMo,capexMo:capexMo,opExMo:opExMo,noiMo:noiMo,cashFlowMo:cashFlowMo,capRate:capRate,coc:coc,initialLtv:initialLtv,breakEven:breakEven,irr:irr,equityMultiple:cashNeeded>0?returned/cashNeeded:NaN,rows:rows};
}
function row(label,value,total){return '<div class="row'+(total?' total':'')+'"><span>'+esc(label)+'</span><span>'+value+'</span></div>';}
function setClass(id,value){var el=byId(id);el.className='v '+(value>=0?'good':'bad');}
function readInputs(){var els=document.querySelectorAll('[data-key]'),s=state.scenarios[state.active],i,key;for(i=0;i<els.length;i++){key=els[i].getAttribute('data-key');s[key]=els[i].type==='text'?els[i].value:n(els[i].value);}save();}
function writeInputs(){var els=document.querySelectorAll('[data-key]'),s=state.scenarios[state.active],i,key;for(i=0;i<els.length;i++){key=els[i].getAttribute('data-key');if(s[key]!==undefined)els[i].value=s[key];}}
function renderTabs(){var html='',i;for(i=0;i<state.scenarios.length;i++)html+='<button class="tab'+(i===state.active?' active':'')+'" data-tab="'+i+'">'+esc(state.scenarios[i].name||('Scenario '+(i+1)))+'</button>';byId('tabs').innerHTML=html;var tabs=document.querySelectorAll('[data-tab]');for(i=0;i<tabs.length;i++)tabs[i].onclick=function(){state.active=parseInt(this.getAttribute('data-tab'),10);save();renderAll();};}
function render(){var s=state.scenarios[state.active],c=calc(s),last=c.rows[c.rows.length-1];byId('resultName').textContent=s.name||'Scenario';byId('holdBadge').textContent=Math.round(n(s.analysisYears))+'-year hold';
 byId('mCashFlow').textContent=money(c.cashFlowMo);setClass('mCashFlow',c.cashFlowMo);byId('mCoc').textContent=pct(c.coc);setClass('mCoc',c.coc);byId('mCap').textContent=pct(c.capRate);byId('mIrr').textContent=pct(c.irr);setClass('mIrr',c.irr);byId('mCashNeeded').textContent=money(c.cashNeeded);byId('mMortgage').textContent=money(c.mortgage);byId('mBreakEven').textContent=money(c.breakEven);byId('mEquityMultiple').textContent=mult(c.equityMultiple);
 byId('opsRows').innerHTML=row('Scheduled rent + income',money(c.grossMo),false)+row('Vacancy allowance','− '+money(c.vacancyMo),false)+row('Effective income',money(c.effectiveMo),true)+row('Operating expenses','− '+money(c.opExMo),false)+row('NOI',money(c.noiMo),true)+row('Mortgage P&I','− '+money(c.mortgage),false)+row('PMI','− '+money(c.pmiMo),false)+row('Cash flow',money(c.cashFlowMo),true);
 byId('financeRows').innerHTML=row('Down payment',money(c.down),false)+row('Loan amount',money(c.loan),false)+row('Closing costs',money(c.closing),false)+row('Renovation',money(n(s.renovation)),false)+row('Initial cash required',money(c.cashNeeded),true)+row('Initial LTV',pct(c.initialLtv),false)+row('Value at exit',money(last.value),false)+row('Mortgage balance at exit',money(last.balance),false)+row('Net sale proceeds',money(last.sale),true);
 var head='<thead><tr><th>Year</th><th>Rent/mo</th><th>NOI</th><th>Cash flow</th><th>Principal</th><th>Interest</th><th>Mortgage bal.</th><th>Property value</th><th>Equity</th><th>Sale proceeds</th><th>Total CF</th></tr></thead><tbody>',body='',i,r;for(i=0;i<c.rows.length;i++){r=c.rows[i];body+='<tr><td>'+r.year+'</td><td>'+money(r.rent)+'</td><td>'+money(r.noi)+'</td><td class="'+(r.cashFlow>=0?'good':'bad')+'">'+money(r.cashFlow)+'</td><td>'+money(r.principal)+'</td><td>'+money(r.interest)+'</td><td>'+money(r.balance)+'</td><td>'+money(r.value)+'</td><td>'+money(r.equity)+'</td><td>'+money(r.sale)+'</td><td>'+money(r.total)+'</td></tr>';};byId('projectionTable').innerHTML=head+body+'</tbody>';
 renderCompare();}
function renderCompare(){var panel=byId('comparePanel');panel.className='card compare'+(state.compare?' show':'');if(!state.compare)return;var labels=['Purchase price','Cash required','Rent / month','Mortgage / month','Cash flow / month','Break-even rent','Cap rate','Cash-on-cash','IRR','Exit equity','Equity multiple'],html='<thead><tr><th>Metric</th>',i,j,c,s;for(i=0;i<state.scenarios.length;i++)html+='<th>'+esc(state.scenarios[i].name)+'</th>';html+='</tr></thead><tbody>';for(j=0;j<labels.length;j++){html+='<tr><td>'+labels[j]+'</td>';for(i=0;i<state.scenarios.length;i++){s=state.scenarios[i];c=calc(s);var vals=[money(n(s.purchasePrice)),money(c.cashNeeded),money(n(s.rentMonthly)),money(c.mortgage),money(c.cashFlowMo),money(c.breakEven),pct(c.capRate),pct(c.coc),pct(c.irr),money(c.rows[c.rows.length-1].equity),mult(c.equityMultiple)];html+='<td>'+vals[j]+'</td>';}html+='</tr>';}html+='</tbody>';byId('compareTable').innerHTML=html;}
function renderAll(){writeInputs();renderTabs();render();byId('compareBtn').textContent=state.compare?'Hide comparison':'Compare scenarios';}
function download(name,text,type){var blob=new Blob([text],{type:type}),url=URL.createObjectURL(blob),a=document.createElement('a');a.href=url;a.download=name;document.body.appendChild(a);a.click();document.body.removeChild(a);setTimeout(function(){URL.revokeObjectURL(url);},1000);}
function bind(){var els=document.querySelectorAll('[data-key]'),i;for(i=0;i<els.length;i++){els[i].addEventListener('input',function(){readInputs();renderTabs();render();});els[i].addEventListener('change',function(){readInputs();renderTabs();render();});}
 byId('addBtn').onclick=function(){readInputs();var s=clone(defaults);s.name='Scenario '+(state.scenarios.length+1);state.scenarios.push(s);state.active=state.scenarios.length-1;save();renderAll();};
 byId('duplicateBtn').onclick=function(){readInputs();var s=clone(state.scenarios[state.active]);s.name=(s.name||'Scenario')+' copy';state.scenarios.push(s);state.active=state.scenarios.length-1;save();renderAll();};
 byId('deleteBtn').onclick=function(){if(state.scenarios.length===1){state.scenarios=[clone(defaults)];state.active=0;}else{state.scenarios.splice(state.active,1);if(state.active>=state.scenarios.length)state.active=state.scenarios.length-1;}save();renderAll();};
 byId('compareBtn').onclick=function(){readInputs();state.compare=!state.compare;save();renderAll();};
 byId('csvBtn').onclick=function(){readInputs();var c=calc(state.scenarios[state.active]),rows=[['year','rent_monthly','gross_income','effective_income','operating_expenses','noi','interest','principal','pmi','cash_flow','property_value','mortgage_balance','equity','sale_proceeds','total_cash_flow']],i,r;for(i=0;i<c.rows.length;i++){r=c.rows[i];rows.push([r.year,r.rent,r.gross,r.effective,r.opEx,r.noi,r.interest,r.principal,r.pmi,r.cashFlow,r.value,r.balance,r.equity,r.sale,r.total]);}download('rental-projection.csv',rows.map(function(x){return x.join(',');}).join('\n'),'text/csv');};
}
window.onerror=function(msg){var st=byId('status');if(st){st.style.display='block';st.textContent='Calculator error: '+msg;}};
load();bind();renderAll();
})();
