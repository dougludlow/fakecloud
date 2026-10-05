<?php

declare(strict_types=1);

// Router script for `php -S`, used by the unit tests to mock the fakecloud
// HTTP API. Each request is recorded to $MOCK_DIR/request.json and answered
// with the status + body the test wrote to $MOCK_DIR/response.json.

$dir = getenv('MOCK_DIR');
file_put_contents($dir . '/request.json', json_encode([
    'method' => $_SERVER['REQUEST_METHOD'],
    'uri' => $_SERVER['REQUEST_URI'],
    'body' => file_get_contents('php://input'),
]));

$response = json_decode((string) file_get_contents($dir . '/response.json'), true);
http_response_code($response['status']);
header('Content-Type: application/json');
echo $response['body'];
