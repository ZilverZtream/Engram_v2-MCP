export function callPageMethods(customerId: string): void {
    var query = document.getElementById('txtQuery') as HTMLInputElement;
    if (query) {
        query.value = customerId;
    }

    PageMethods.GetCustomer(customerId, function () {
        __doPostBack('btnSearch', 'fromPageMethods');
    });
}
