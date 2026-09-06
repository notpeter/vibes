(function () {
'use strict';

var info = {
  name: {
    title: 'Scenario name',
    description: 'A label for this scenario so you can distinguish it from other saved scenarios. It has no effect on any calculation.'
  },
  purchasePrice: {
    title: 'Purchase price',
    description: 'The acquisition price of the property before buyer closing costs or renovation. The calculator uses this as the basis for the down payment, closing costs, initial LTV, year-1 cap rate, and future appreciation.'
  },
  downPaymentPct: {
    title: 'Down payment',
    description: 'The percentage of the purchase price paid in cash at acquisition. The initial mortgage principal is purchase price minus this down payment.'
  },
  closingCostsPct: {
    title: 'Closing costs',
    description: 'Estimated buyer closing costs as a percentage of purchase price. These are treated as upfront cash invested and are not financed in the mortgage.'
  },
  renovation: {
    title: 'Renovation / upfront work',
    description: 'Cash spent on repairs, improvements, make-ready work, or other projects at acquisition. It increases initial cash required. In this model it does not automatically increase the property’s starting value or rent; change those inputs separately if renovation changes them.'
  },
  mortgageRate: {
    title: 'Mortgage interest rate',
    description: 'The nominal annual interest rate used to calculate a fixed-rate, fully amortizing mortgage payment and the year-by-year principal balance.'
  },
  mortgageTerm: {
    title: 'Mortgage term',
    description: 'The amortization period of the mortgage in years. A longer term generally lowers the monthly payment but slows principal paydown and increases total interest.'
  },
  pmiPct: {
    title: 'PMI annual rate',
    description: 'Annual private mortgage insurance cost as a percentage of the original loan amount. The calculator divides this by 12 for a monthly PMI estimate unless a monthly override is entered.'
  },
  pmiMonthlyOverride: {
    title: 'PMI monthly override',
    description: 'An exact monthly PMI amount. Any value above $0 replaces the PMI annual-rate calculation. Leave this at $0 to use the annual PMI rate instead.'
  },
  pmiCancelLtv: {
    title: 'PMI cancellation LTV',
    description: 'The loan-to-value threshold at or below which the model stops charging PMI. LTV is calculated from the remaining mortgage balance divided by the modeled property value, so appreciation can cause modeled PMI to end sooner.'
  },
  rentMonthly: {
    title: 'Monthly rent',
    description: 'Scheduled base rent in year 1 before vacancy or credit loss. Management, maintenance, and CapEx percentage expenses are calculated from this rent, and future rent grows at the annual rent-increase assumption.'
  },
  otherIncomeMonthly: {
    title: 'Other monthly income',
    description: 'Recurring income other than base rent, such as parking, laundry, storage, pet rent, or garage rent. Vacancy is applied to it, and it grows at the same annual rate as rent.'
  },
  vacancyPct: {
    title: 'Vacancy / credit loss',
    description: 'The percentage of scheduled rent plus other income assumed to be lost to vacancy, turnover, or nonpayment. The same percentage is applied in every projection year.'
  },
  rentGrowthPct: {
    title: 'Annual rent increase',
    description: 'The compound annual growth rate applied to both base rent and other income. Year 1 uses the entered amounts; each later year grows from the prior year.'
  },
  propertyTaxAnnual: {
    title: 'Property taxes — annual',
    description: 'Year-1 annual real-estate taxes paid by the owner. In later years this amount grows at the annual expense-increase assumption.'
  },
  insuranceAnnual: {
    title: 'Insurance — annual',
    description: 'Year-1 annual property or landlord insurance cost. In later years it grows at the annual expense-increase assumption.'
  },
  hoaMonthly: {
    title: 'HOA — monthly',
    description: 'Year-1 monthly condominium, HOA, or association dues paid by the owner. The annualized amount grows at the annual expense-increase assumption.'
  },
  managementPct: {
    title: 'Management',
    description: 'Property-management cost as a percentage of scheduled base rent. It is not charged against other income in this model. Because it is rent-based, it rises automatically when modeled rent rises.'
  },
  maintenancePct: {
    title: 'Maintenance reserve',
    description: 'A reserve for routine repairs and maintenance, expressed as a percentage of scheduled base rent. This is a budgeting assumption rather than an itemized repair forecast and rises automatically with rent.'
  },
  capexPct: {
    title: 'CapEx reserve',
    description: 'A reserve for larger, long-lived replacements such as roofs, HVAC equipment, appliances, paving, or major building systems, expressed as a percentage of scheduled base rent. It rises automatically with rent.'
  },
  utilitiesMonthly: {
    title: 'Owner-paid utilities — monthly',
    description: 'Year-1 monthly utilities that remain the owner’s responsibility, such as water, sewer, heat, electricity, trash, or common-area service. This amount grows at the annual expense-increase assumption.'
  },
  otherExpensesMonthly: {
    title: 'Other expenses — monthly',
    description: 'A catch-all year-1 monthly operating expense for recurring costs not represented elsewhere. It grows at the annual expense-increase assumption.'
  },
  expenseGrowthPct: {
    title: 'Annual expense increase',
    description: 'The compound annual growth rate applied to property taxes, insurance, HOA dues, owner-paid utilities, and other fixed expenses. Management, maintenance, and CapEx are not separately inflated because they are percentages of rent and therefore already rise with rent.'
  },
  analysisYears: {
    title: 'Hold period',
    description: 'The number of years you plan to own the property. The projection assumes the property is sold at the end of the final year; this determines the exit value, mortgage payoff, sale proceeds, and IRR period.'
  },
  appreciationPct: {
    title: 'Annual appreciation',
    description: 'The compound annual growth rate applied to the purchase price to estimate future property value. The model does not add renovation spending to the starting valuation automatically.'
  },
  sellingCostsPct: {
    title: 'Selling costs',
    description: 'The percentage of the modeled exit property value deducted for broker commissions and other sale costs before the remaining mortgage balance is paid off.'
  }
};

window.RENTAL_FIELD_INFO = info;

function addStyles() {
  var style = document.createElement('style');
  style.textContent =
    '.info-link{display:inline-flex;align-items:center;justify-content:center;width:16px;height:16px;margin-left:5px;vertical-align:-2px;border:1px solid #98a2b3;border-radius:50%;color:#667085;font-size:10px;font-weight:700;line-height:1;text-decoration:none;background:#fff}' +
    '.info-link:hover,.info-link:focus{border-color:#3157d5;color:#3157d5;background:#eef2ff;outline:none}' +
    '.help-shell{max-width:900px;margin:0 auto;padding:24px}' +
    '.help-top{display:flex;align-items:flex-start;justify-content:space-between;gap:16px;margin-bottom:18px}' +
    '.help-top h1{margin:0;font-size:27px}.help-top p{color:#667085;margin:5px 0 0}' +
    '.help-back{white-space:nowrap}' +
    '.help-list{display:grid;gap:10px}' +
    '.help-entry{scroll-margin-top:16px;background:#fff;border:1px solid #dfe3ea;border-radius:10px;padding:14px}' +
    '.help-entry:target{border-color:#3157d5;box-shadow:0 0 0 3px #eef2ff}' +
    '.help-entry h2{font-size:15px;margin:0 0 5px}.help-entry p{margin:0;color:#475467;line-height:1.5}' +
    '@media(max-width:620px){.help-shell{padding:12px}.help-top{display:block}.help-back{display:inline-block;margin-top:10px}}';
  document.head.appendChild(style);
}

function addInfoLinks() {
  var inputs = document.querySelectorAll('[data-key]');
  for (var i = 0; i < inputs.length; i++) {
    var key = inputs[i].getAttribute('data-key');
    var item = info[key];
    if (!item) continue;
    var field = inputs[i].closest ? inputs[i].closest('.field') : null;
    if (!field || field.querySelector('.info-link[data-info-key="' + key + '"]')) continue;
    var label = field.querySelector('label');
    if (!label) continue;
    var link = document.createElement('a');
    link.className = 'info-link';
    link.setAttribute('data-info-key', key);
    link.href = 'descriptions.html#field-' + key;
    link.target = 'rental-field-help';
    link.rel = 'noopener';
    link.textContent = 'i';
    link.title = item.title + ' — description';
    link.setAttribute('aria-label', 'Description of ' + item.title + ' (opens field guide)');
    label.insertAdjacentElement('afterend', link);
  }
}

function renderDescriptions() {
  var list = document.getElementById('fieldDescriptions');
  if (!list) return;
  var order = [
    'name',
    'purchasePrice','downPaymentPct','closingCostsPct','renovation','mortgageRate','mortgageTerm','pmiPct','pmiMonthlyOverride','pmiCancelLtv',
    'rentMonthly','otherIncomeMonthly','vacancyPct','rentGrowthPct',
    'propertyTaxAnnual','insuranceAnnual','hoaMonthly','managementPct','maintenancePct','capexPct','utilitiesMonthly','otherExpensesMonthly','expenseGrowthPct',
    'analysisYears','appreciationPct','sellingCostsPct'
  ];
  var html = '';
  for (var i = 0; i < order.length; i++) {
    var key = order[i];
    var item = info[key];
    html += '<section class="help-entry" id="field-' + key + '"><h2>' + escapeHtml(item.title) + '</h2><p>' + escapeHtml(item.description) + '</p></section>';
  }
  list.innerHTML = html;
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, function (c) {
    return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];
  });
}

addStyles();
addInfoLinks();
renderDescriptions();
})();
