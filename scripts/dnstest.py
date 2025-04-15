import socks
import socket
import dns.message
import dns.query
import dns.exception
import time

# Configuring SOCKS5 proxy settings
proxy_host = '127.0.0.1'  # Replace with your SOCKS5 proxy host
proxy_port = 1089  # Replace with your SOCKS5 proxy port
# proxy_host = '192.168.7.1'  # Replace with your SOCKS5 proxy host
# proxy_port = 1086  # Replace with your SOCKS5 proxy port
proxy_user = None  # Set this if your proxy requires authentication
proxy_pass = None  # Set this if your proxy requires authentication

# Target DNS server (can be any public DNS resolver)
dns_server = 'dns.google.com'  # Google DNS
dns_port = 53  # DNS uses port 53 for queries

# Create a SOCKS5 proxy connection
socks.set_default_proxy(socks.SOCKS5, proxy_host, proxy_port, True, proxy_user, proxy_pass)
socket.socket = socks.socksocket

# Function to perform DNS query via SOCKS5 UDP proxy
def dns_query_via_proxy(domain):
    try:
        # Create a DNS query message
        query = dns.message.make_query(domain, dns.rdatatype.ANY)

        # Send the query to the DNS server via SOCKS5 proxy using a custom UDP socket
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.settimeout(5)  # Timeout for the request
            
            # Send the DNS query
            sock.sendto(query.to_wire(), (dns_server, dns_port))

            # Receive the response
            response, addr = sock.recvfrom(512)  # DNS responses are usually <= 512 bytes
            
            # Parse the DNS response using dnspython
            response_message = dns.message.from_wire(response)
            
            print(f"Response received for {domain}:")
            print(response_message)
    
    except dns.exception.DNSException as e:
        print(f"DNS query failed: {e}")
    except socket.timeout:
        print("No response received within the timeout period.")
    except Exception as e:
        print(f"Error: {e}")

if __name__ == '__main__':
    domain_to_query = 'www.baidu.com'  # Domain you want to query
    print(f"Performing DNS query for '{domain_to_query}' via SOCKS5 proxy...")
    dns_query_via_proxy(domain_to_query)
