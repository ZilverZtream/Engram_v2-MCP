namespace LegacyApp.Api
{
    public class CustomerApiController
    {
        public string Search(string customerId)
        {
            return new LegacyApp.CustomerDal().LookupCustomer(customerId);
        }
    }
}
