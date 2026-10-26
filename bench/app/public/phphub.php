<?php
$app = require dirname(__DIR__) . '/bootstrap.php';

$served = 0;
while (phphub_handle_request()) {
    header('X-Served: ' . ++$served);
    $app->handle($_SERVER['REQUEST_URI'])->send();
    phphub_finish_request();
}
