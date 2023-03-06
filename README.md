# FastCGI request simulator CLI tool

Default behavior is to act like a CGI/1.1 script, and forward the request to
the FastCGI server at the address specified on the command line.

Use the -h, --help option to get more information on usage and available
options.

## Features
- CGI/1.1 to FastCGI bridge
- Simulate any FastCGI request from command line
- Command line interface designed to be familiar to cURL users
- Can connect to server over either TCP or Unix domain socket
- Environment variables that correspond to CGI/1.1 meta- or protocol variables
  are passed to the server as FastCGI parameters automatically.
  You may whitelist other environment variables as you wish.
- Allows to specify full URL of request to simulate, and sets `HTTP_HOST`,
  `HTTPS`, `PATH_INFO`, `QUERY_STRING` and `REQUEST_URI` accordingly.
- If given a script name, removes a matching prefix from `PATH_INFO`.
- Allows to specify server document root, and tries to set `PATH_TRANSLATED`
  and `SCRIPT_FILENAME` accordingly.
- Sends input from stdin as request body data if `CONTENT_LENGTH` is specified.
- Does not validate your request, just passes it on.
- May still fail on you if it happens to consume your garbage.
  Most of the time, you can work around this by avoiding the feature causing
  the error. E.g. use environment variables instead of options and/or explicit
  URL.

## Example
Call the PHP-FPM ping endpoint over Unix domain socket:

```
SCRIPT_NAME='/ping' fcgi -f --root /var/www/html /run/php-fpm/www.sock
```