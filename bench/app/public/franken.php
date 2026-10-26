<?php
$app = require dirname(__DIR__) . '/bootstrap.php';

$served = 0;
$handler = static function () use ($app, &$served) {
    header('X-Served: ' . ++$served);
    $app->handle($_SERVER['REQUEST_URI'])->send();
};
while (frankenphp_handle_request($handler)) {
}
