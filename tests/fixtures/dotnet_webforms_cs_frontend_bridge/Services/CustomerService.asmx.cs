using System.Web.Services;

namespace LegacyApp.Services
{
    [WebService(Namespace = "http://tempuri.org/")]
    public class CustomerService : WebService
    {
        [WebMethod]
        public string LookupCustomer(string customerId)
        {
            return new LegacyApp.CustomerDal().LookupCustomer(customerId);
        }
    }
}
