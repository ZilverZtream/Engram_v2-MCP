(function () {
    function wireLegacyButtons() {
        var queryInput = document.getElementById('txtQuery');
        queryInput.value = 'ALFKI';

        // Frontend trigger that causes a server postback event.
        __doPostBack('btnSearch', 'quick');
    }

    function hitLegacyServices() {
        $.ajax({
            url: 'Services/CustomerService.asmx/LookupCustomer',
            type: 'POST',
            data: JSON.stringify({ customerId: 'ALFKI' })
        });

        fetch('/api/customer/search?customerId=ALFKI');
    }

    window.legacySearch = function () {
        wireLegacyButtons();
        hitLegacyServices();
    };
})();
